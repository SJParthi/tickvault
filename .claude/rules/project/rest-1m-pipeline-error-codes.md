# Per-Minute REST 1m Pipeline — Error Codes (SPOT1M-01 / SPOT1M-02 / CHAIN-01..04)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `no-rest-except-live-feed-2026-06-27.md` §8 (the 2026-07-12 operator grant —
> the §8 text lands in the parallel rules PR; this runbook references it as
> the authorizing edit) > `live-feed-purity.md` (the `ticks` table is
> untouched — this pipeline writes ONLY its own `spot_1m_rest` table) > this
> file.
> **Operator directive (2026-07-12):** every trading-day minute close in
> session (the 09:15 candle closes at 09:16:00 IST; the last, 15:29 candle
> closes at 15:30:00), within ~1s, fetch that just-closed minute's official
> 1m OHLCV for the 3 IDX_I spot indices — NIFTY 13, BANKNIFTY 25, SENSEX 51 —
> via Dhan `POST /v2/charts/intraday` (interval "1") and persist to the new
> `spot_1m_rest` QuestDB table. Cold path only; the WS candle pipeline is
> untouched.
> **Companion code:** `crates/app/src/spot_1m_rest_boot.rs` (scheduler +
> fetch ladder + edge escalation + supervised respawn),
> `crates/storage/src/spot_1m_rest_persistence.rs` (DDL + ILP-over-HTTP
> writer), `crates/common/src/constants.rs` (`SPOT_1M_REST_*` constants),
> `crates/common/src/config.rs::Spot1mRestConfig` (`[spot_1m_rest]`,
> fail-safe default OFF; `config/base.toml` opts in),
> `crates/common/src/error_code.rs::ErrorCode::{Spot1m01FetchDegraded,
> Spot1m02PersistFailed}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `Spot1m0*` and `Chain0*` variant
> verbatim — `SPOT1M-01`, `SPOT1M-02`, `CHAIN-01`, `CHAIN-02`, `CHAIN-03`
> and `CHAIN-04` appear below.
> **PR-3 companion code (the OPTION-CHAIN half, appended 2026-07-12):**
> `crates/app/src/option_chain_1m_boot.rs` (expirylist warmup + entitlement
> probe + spot-sequenced per-minute chain scheduler + supervised respawn),
> `crates/storage/src/option_chain_1m_persistence.rs` (DDL + ILP-over-HTTP
> writer), `crates/common/src/constants.rs` (`CHAIN_1M_*` +
> `DHAN_OPTION_CHAIN_*` constants),
> `crates/common/src/config.rs::OptionChain1mConfig` (`[option_chain_1m]`,
> serde DEFAULT-OFF fail-safe; base.toml `enabled = true` since 2026-07-13
> — the live probe PASSED, see `no-rest-except-live-feed-2026-06-27.md`
> §8.7; `probe_and_report` default ON), `crates/common/src/error_code.rs::ErrorCode::{
> Chain01EntitlementAbsent, Chain02FetchDegraded, Chain03PersistFailed,
> Chain04ExpirylistFailed}`.

---

## §0. Why these codes exist

The per-minute spot pipeline is a scheduled REST fetcher: at every minute
close in `[09:16:00, 15:30:00]` IST (inclusive; trading days only) it wakes
~300 ms after the boundary and pulls the JUST-CLOSED minute's official 1m
candle for the 3 spot indices, persisting to `spot_1m_rest` (DEDUP
`(ts, security_id, exchange_segment, feed)` — idempotent re-appends). Dhan
does NOT document how quickly the just-closed minute becomes available, so
each fetch carries a bounded in-minute re-poll ladder (~0.7s / 1.5s / 3s /
6s after the first attempt) and the `tv_spot1m_close_to_data_ms` histogram
is the honest live probe of that latency. Every failure class below is a
DEGRADE — the live WS candle pipeline, tick capture, and trading are NEVER
affected.

**2026-07-13 first-live-session hotfix (same-day):** the spot fetcher's
original same-date `[minute open, open+60s]` request window was answered
`2xx` WITHOUT the target candle for EVERY session minute (SPOT1M-01
`ok=0/errors=0/empty=3` from 09:16 IST; the matcher itself was verified
correct). Each fire now sends the ONLY live-proven window shape — the
day-granular `fromDate = D 00:00:00, toDate = D+1 00:00:00` body the 15:31
cross-verify uses — filtered client-side to the exact minute, PLUS a
previous-minute BACKFILL sweep: each fire also persists the previous
minute when it was not successfully persisted (per-SID in-memory
watermark, committed only after a confirmed flush; DEDUP-idempotent
re-appends; `tv_spot1m_backfilled_total` counts repairs). Edge honesty:
a fire's verdict is its OWN target minute — a minute that lands only via
next-fire backfill was still that fire's failure, and the backfilled
row's `close_to_data_ms` column stamps the REAL (> 60 s) retrieval delay
while the histogram keeps sampling own-fire retrievals only. See
`no-rest-except-live-feed-2026-06-27.md` §8.7 for the dated record.

**2026-07-13 429-coordination follow-up (same-day, second PR):** the first
live session ALSO showed `/v2/charts/intraday` rate-limiting BOTH
consumers — the 15:31 bulk cross-verify lost 91/776 fetches to HTTP 429 at
15:31–15:33 (compared=0, a BLIND day). Three bounded changes: (a) the spot
half's ONE post-session repair sweep now fires at **~15:33:30 IST** (was
~15:31:00) so its ≤3 requests clear that burst window (const-asserted ≥
the cross-verify trigger + 150 s and before the 16:30 IST box stop);
(b) the in-minute re-poll ladder carries a deterministic per-SID schedule
jitter (slot × 150 ms — 0/150/300 ms, NO randomness) so the 3 concurrent
ladders never re-poll in lockstep, and an HTTP 429 adds a bounded +2 s
backoff before the NEXT rung (same rung count — never an extra retry;
still counted by `tv_spot1m_rate_limited_total`; the worst-case all-429
jittered schedule of 19.3 s is const-asserted inside the 20 s per-SID
budget); (c) the cross-verify gained its OWN bounded 429 second pass —
see the dated note in `cross-verify-1m-error-codes.md` §2 and the
`tv_cross_verify_1m_retry_429_total{outcome}` counters.

The OPTION-CHAIN half (PR-3, appended 2026-07-12) shares the same minute
boundaries, SEQUENCED immediately after the spot leg via a watch signal the
spot task publishes at the end of each fire (fallback timer 2.5 s after the
boundary — the chain is never blocked by a disabled/dead/slow spot leg). It
SHIPPED config-gated **DEFAULT-OFF** pending a live entitlement probe (the
account had NO Option Chain Data-API entitlement in June 2026 — DH-902/806
class — and the entitlement is unprobeable from the dev sandbox); the
first-live-boot probe **PASSED at 08:31:49 IST on 2026-07-13** ("entitlement
probe PASSED — chain data is available", NIFTY, 18 expiries) and
`[option_chain_1m].enabled` is **true in base.toml since 2026-07-13** per
the dated note in `no-rest-except-live-feed-2026-06-27.md` §8.7 (the serde
DEFAULT stays off — fail-safe): while
disabled, `probe_and_report` (default ON) runs ONE boot-time expirylist
probe and reports the verdict via Telegram; the pipeline NEVER auto-runs —
the operator flips `[option_chain_1m].enabled`. When enabled: a day-start
`POST /v2/optionchain/expirylist` warmup pins each underlying's CURRENT
expiry (nearest ≥ today; on expiry day the same-day expiry holds through
the session; NEVER guessed — option-chain.md rule 9), then each minute
close pulls the FULL current-expiry chain per underlying
(`POST /v2/optionchain`, `client-id` header, 1-unique-request-per-3s limit
honored trivially at one request per underlying per minute + a defensive
≥3 s per-underlying min-gap guard) and persists every per-strike per-leg
row to `option_chain_1m` (DEDUP `(ts, underlying_security_id,
exchange_segment, expiry, strike, leg, feed)`).

## §1. SPOT1M-01 — per-minute spot fetch degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; the next minute boundary re-attempts automatically — the operator
inspects, never manually re-fetches first).

**Trigger:** one of (`ErrorCode::Spot1m01FetchDegraded`, distinguished by
the `stage` field):

1. `stage="minute_failed"` — coalesced ONCE per fired minute when one or
   more of the 3 SIDs ended the bounded ladder without the target candle:
   transport error, non-2xx (incl. DH-904/429 — counted by
   `tv_spot1m_rate_limited_total`, +2 s bounded backoff before the next
   rung since the 2026-07-13 429-coordination follow-up, never retried
   past the ladder), no
   token at fire time, or a 200 whose body never contained the just-closed
   minute (`outcome="empty"` — counted, included in the failure edge,
   never silent per audit Rule 11). Sub-edge: log-only, never a page.
2. `stage="escalation"` — the EDGE: 3 consecutive fully-failed minutes.
   Since the 2026-07-12 hostile-review M1 fix, "fully failed" = no SID
   succeeded **OR the persist leg (append/flush) failed** — a day-long
   QuestDB outage therefore pages through THIS edge (fetch-ok-but-lost
   rows are not "ok"). Fires ONCE per episode + the typed HIGH Telegram
   event; re-armed only after a minute where the fetch AND persist both
   succeed (recovery = one Info Telegram).
3. `stage="client_build"` / `stage="task_respawn"` — the long-lived HTTP
   client could not be built (HTTP-CLIENT-01 class — host fd/TLS/resolver
   pressure) or the scheduler task died and the supervisor respawned it
   (`tv_spot1m_task_respawn_total{reason}`).
4. `stage="boundary_skipped"` (2026-07-12 H2 fix) — one or more minute
   boundaries elapsed UNFETCHED (a fire overran its minute, or a
   suspend/clock step swallowed boundaries). Counted by
   `tv_spot1m_boundary_skipped_total`, coalesced to ONE coded log, and
   each missed minute FEEDS the failure edge (a sustained-overrun outage
   still reaches the escalation page). Overruns are structurally bounded:
   each SID's whole ladder is `tokio::time::timeout`-bounded by
   `SPOT_1M_REST_SID_BUDGET_SECS` (20 s) with a 5 s per-request timeout
   (`SPOT_1M_REST_REQUEST_TIMEOUT_SECS`), const-asserted < the minute —
   `tv_spot1m_sid_budget_exceeded_total` counts budget trips.
5. `stage="sid_not_served"` (2026-07-13 — INDIA VIX joins the spot set;
   operator scope addition 2026-07-13, relayed via the coordinator
   session: INDIA VIX joins the spot 1m pull, spot only, no option
   chain) — the per-SID persistent-empty detector: ONE SID accumulated
   `SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD` (10) consecutive empty/failed
   minutes WHILE ≥1 other SID succeeded in those same minutes — the
   vendor is not serving THIS index (a global-outage minute neither
   counts nor resets the streak; general outages stay the
   `stage="escalation"` edge's page). Fires ONE edge-latched HIGH page
   per SID per episode (typed `Spot1mSidNotServed` Telegram, plain
   English: "Dhan is not returning 1-minute candles for INDIA VIX — the
   other indices are unaffected"), re-armed only by that SID's own
   recovery (one Info `Spot1mSidServedRecovered`). Counter:
   `tv_spot1m_sid_not_served_total{symbol}` (4 static label values —
   the pinned index symbols), one increment per counted not-served
   minute. HONESTY: whether Dhan `/v2/charts/intraday` serves INDIA VIX
   1m candles at all is a LIVE-PROBE UNKNOWN — the spot set is now 4
   SIDs (NIFTY 13 / BANKNIFTY 25 / SENSEX 51 / INDIA VIX 21; still
   inside the Data-API 5/sec budget, jitter slots widened 0/150/300/450
   ms, worst-case ladder 19.45 s < the 20 s budget), the chain leg stays
   the VIX-free 3-underlying `CHAIN_1M_UNDERLYINGS` subset
   (const-asserted — VIX can never enter the option-chain pipeline),
   per-SID independence is unit-pinned (a 3-ok/1-empty minute is NOT
   fully-failed and NOT edge-counted), and index candles legitimately
   carry zero volume (never flagged as an error).

**2026-07-14 update — serving-delay diagnostics (empty-class split + one-shot
probes):** the 2026-07-14 morning ran 21/21 minutes `empty` on all 4 SIDs
(2xx, no errors, no 429s, healthy token) WITH the #1499 day-granular window
live — and the single `outcome="empty"` label could not discriminate the two
very different vendor states behind it. The split (unconditional — honest
accounting): `outcome="empty_no_rows"` (the 2xx body parsed to ZERO candles
for the whole day) vs `outcome="empty_stale"` (candles present but none at
the target minute — the vendor is serving the day with a LAG). Every
`empty_stale` records the measured SERVING LAG (`target minute open −
newest candle minute open`, whole seconds) into the
`tv_spot1m_serving_lag_ms` histogram (dedicated 1 s→6 h buckets via
`Matcher::Full` in `observability.rs` — the generic `_ms` 60 s cap would
collapse every meaningful sample into `+Inf`), and the coalesced
`stage="minute_failed"` line gains `empty_no_rows` / `empty_stale` /
`rows_in_response` / `last_candle_ist` / `max_serving_lag_secs` fields —
one glance per minute answers "is Dhan behind, and by how much?". Edge /
backfill / persist semantics are UNCHANGED (both empty classes still count
as an empty minute). Companion LOG-ONLY probes, config-gated
(`[spot_1m_rest] diagnostics`, serde default OFF; base.toml ON while this
investigation runs): two one-shot moments per day (the first session fire
after boot + a configurable second instant, default 11:00 IST —
`diagnostics_second_probe_secs_of_day_ist`), each issuing ≤3 bounded extra
requests for ONE SID ~300 ms apart (≤6/day, inside the Data-API 5/sec
budget; a probe only starts with ≥20 s of room before the next boundary and
otherwise defers) — (a) the 15:31 cross-verify's BYTE-EXACT day window
(equality unit-pinned: both builders share `intraday_request_body`), (b)
the previous-trading-day full window (proves settled-data serving), (c) a
same-day window with `toDate = now`. All three requests' bodies + response
shapes (rows, first/last candle IST, target presence, serving lag) land
side by side in ONE structured `info!` line
("spot_1m_rest diagnostics: one-shot serving-delay probe"). Together with
the 15:33:30 sweep's verdict this discriminates "Dhan serves same-day
intraday candles with a DELAY" from "our request shape is wrong". The
probes never touch the fetch / persist / edge legs.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `SPOT1M-01`; the payload
   carries `stage`, the failing minute (IST), per-SID outcomes and a
   bounded secret-redacted sample failure reason (status + redacted URL +
   ≤300-char body via the house `capture_rest_error_body`).
2. Cross-check the REST canary (REST-CANARY-01) and the WS feed
   (`tv_websocket_connections_active`): WS healthy + this failing = a
   REST-surface problem (data plan, token, Dhan gateway); both failing =
   network/token — see the AUTH-GAP runbooks.
3. A sustained `outcome="empty"` rate with 2xx responses means Dhan seals
   the just-closed minute SLOWER than the ~6.3s ladder — read the
   `tv_spot1m_close_to_data_ms` histogram and bring the measured latency
   to the operator before touching the ladder constants.
4. `tv_spot1m_fetch_total{outcome="ok"|"empty"|"error"}` rates name the
   dominant class; `tv_spot1m_task_respawn_total{reason}` flapping means a
   real bug — capture the panic backtrace in `data/logs/errors.jsonl.*`.

**Honest envelope:** the fetcher guarantees a bounded, loud attempt per
minute — it cannot force Dhan to serve the candle. Missed minutes leave a
GAP in `spot_1m_rest` (never fabricated rows); the next boundary
re-attempts, and a later manual re-run of the day is possible because
re-appends UPSERT in place. Release-build panics abort the process
(`panic = "abort"`) — the respawn arms are unwind-build/self-heal paths,
the same honesty note as TICK-FLUSH-01.

**2026-07-14 — once-per-day RAW-BODY SAMPLE (`stage="raw_body_sample"`) —
the account-condition vs envelope-drift discriminator:** for ≥14 days BOTH
`/v2/charts/intraday` AND `/v2/charts/historical` returned 2xx with ZERO
parseable candles for ALL SIDs (the 2026-07-14 session ended
`swept=0, still_missing=1500` — the ENTIRE day absent for all 4 spot
indices) while the option-chain leg and the WS feed on the SAME
token/account worked — the account-wide 200-empty verdict. No 2xx body was
ever logged anywhere, so "Dhan serves an empty-but-well-formed columnar
envelope" (account/data-plan condition — Dhan-support territory) was
indistinguishable from "the envelope shape drifted and our parser sees
nothing" (our bug). The FIRST `empty_no_rows`/`empty_stale` classification
of each IST day now logs ONE structured `error!` (`SPOT1M-01`,
`stage="raw_body_sample"`) carrying a bounded 600-char SECRET-REDACTED
sample of the 2xx body (`capture_rest_raw_body_sample` — the
`capture_rest_error_body` pipeline with the widened
`REST_RAW_BODY_SAMPLE_MAX_CHARS` bound), the total body byte length, and
the `Content-Type` header value. Edge-latched (CAS on an IST-day key —
exactly one line per day per process, unconditional); the #1524
diagnostics probes additionally carry `content_type`/`body_bytes`/
`body_sample` per probe entry (diagnostics-gated by construction); the
15:31 cross-verify's `dhan_intraday_fetch` mirrors the capture once per
RUN under `CROSS-VERIFY-1M-02` `stage="raw_body_sample"` (dormant while
the Dhan lane is retired — the 15:31 spawn is lane-only per the
2026-07-13 retirement, so it rides any forced/re-enabled run). This line
is the verbatim evidence block for the Dhan support ticket.

**2026-07-13 — the GROWW spot leg emits this SAME code (no new variant):**
the Groww per-minute spot 1m REST leg
(`crates/app/src/groww_spot_1m_boot.rs`, operator grant 2026-07-13 —
`groww-second-feed-scope-2026-06-19.md` §38 /
`no-rest-except-live-feed-2026-06-27.md` §9, plan
`.claude/plans/active-plan-groww-rest-1m.md` PR-2) reuses `SPOT1M-01` with
the SAME stage taxonomy, distinguished by a **`feed = "groww"` field on
every emit** (the Dhan sites stay field-less — grep `feed="groww"` to
split). Groww-specific additions: `stage="token_read"` (the shared-minter
SSM read failed — re-read paced ≥60 s, NEVER minted, per
`groww-shared-token-minter-2026-07-02.md`) and the sweep stages
(`sweep_failed`/`sweep_incomplete` — the #1499 post-session repair sweep,
shipped in the Groww leg from day one alongside the day-granular window +
the one-minute-lookback backfill). Groww counters mirror the Dhan names
under the `tv_groww_spot1m_*` prefix (`fetch_total{outcome}`,
`close_to_data_ms`, `rate_limited_total`, `boundary_skipped_total`,
`task_respawn_total{reason}`, `backfilled_total`, `sweep_*`,
`ts_form_total{form}` — the UNVERIFIED-LIVE timestamp wire-format probe).
The typed pages are the Groww-specific `GrowwSpot1mFetchDegraded` /
`GrowwSpot1mFetchRecovered` Telegram events (same 3-minute edge).

**2026-07-13 scope note — the Groww spot leg covers 4 indices (INDIA VIX
added, SPOT ONLY; `groww-second-feed-scope-2026-06-19.md` §38.7):** the
4th target's Groww identity is RUNTIME-resolved from the day's watch file
(never a guessed literal). VIX-specific stages on `SPOT1M-01`
(`feed="groww"`, both `warn!`-level, log-sink-only per §3):
`stage="vix_unresolved"` — the day's master carries no resolvable VIX row
(one edge-latched warn per run; `tv_groww_spot1m_vix_unresolved_total` per
attempt; VIX skipped, the 3 core indices unaffected) — and
`stage="vix_not_served"` — the once-per-session sweep found ZERO persisted
VIX minutes while the core indices persisted ("India VIX not served by
Groww historical-candles"; `tv_groww_spot1m_vix_not_served_total`, plus
`tv_groww_spot1m_vix_empty_total` per 2xx-without-the-minute VIX ladder).
Per-SID independence: the 3-minute escalation edge keys on the 3 CORE
indices only — a VIX-only failure never pages, core-all-failed still does.

**2026-07-14 — Groww 2xx GA-FAILURE misclassification fixed (empty →
error) + `ga_code` forensics (coordinator-authorized 2026-07-14 build,
G1):** a 2xx whose body is the Groww FAILURE envelope
(`{"status":"FAILURE","error":{code,message}}` — GA000/GA001/GA003–GA007,
`docs/groww-ref/16-orders-margins-portfolio.md` §5; the envelope wins over
the HTTP status) previously rode the Groww SPOT leg's benign
`outcome="empty"` class. It now classifies `outcome="error"` (feeds the
minute_failed coalesced log, the escalation edge, and a named
`rest_fetch_audit` row with `error_class="ga_failure"`), and the coded
`SPOT1M-01` verdict lines carry a `ga_code` field (`"none"` when absent) +
a redacted bounded message sample. The CHAIN leg (already error-classified)
gains the same `ga_code=` forensics in its parse-failure msg. FORENSICS
ONLY — policy never branches on the GA code (no short-circuit even on
GA005; auth short-circuit stays HTTP-401/403-only). The spot SWEEP arm
(~15:31 IST) applies the SAME sniff — a sweep-time GA FAILURE classifies
`sweep_failed` with the real 200 status + ga_code, never dressed as vendor
absence (`named_gap`). The CONTRACT leg is now the ONLY consumer of the
shared parser's FAILURE-empty return — flagged follow-up.

**2026-07-13 — the GROWW CONTRACT leg emits this SAME code with
`leg = "contract_1m"` (no new variant):** the per-minute per-contract 1m
candle leg (`crates/app/src/groww_contract_1m_boot.rs`, operator grant
2026-07-13 — `groww-second-feed-scope-2026-06-19.md` §38 /
`no-rest-except-live-feed-2026-06-27.md` §9, plan PR-4 — the FILL-MODEL
leg) reuses `SPOT1M-01` (same candles-fetch semantics: sealing-minute
target, day-granular window + client-side minute filter, persist-gated
3-minute failure edge) with **`feed = "groww"` + `leg = "contract_1m"`
fields on every emit** — grep the leg field to split it from the spot
legs. Contract-specific stages beyond the spot taxonomy:
`selection_unresolved` (no chain anchor for an underlying this minute —
its contracts skip; an ATM is never guessed), `anchor_stale` (round-2,
2026-07-13: a chain anchor OLDER than
`GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES` = 5 — the chain leg dead or
frozen past its own 3-minute paging edge — makes the underlying
UNRESOLVED for the minute: counter + ONE edge-latched coded warn per
episode + named audit rows; a frozen off-ATM window is never fetched
silently — the §38.7 decision-freshness principle applied to the
selection input), `selection_truncated` (the
ATM window exceeded the hard `GROWW_CONTRACT_1M_MAX_PER_MINUTE` cap —
truncated deterministically nearest-ATM-first, counted, never fetched
past the cap), `book_unresolved` (warmup — the instruments master gave no
usable contracts at the current expiry; that underlying degrades for the
day, contract identities are never guessed), `token_collision` (warmup —
a duplicate `exchange_token` across DIFFERENT contracts in the master:
later rows dropped keep-first + counter + one coded warn, the Dhan
dedup-drop precedent), `enabled_without_chain` (boot — the contract leg
enabled without the chain leg; refused loudly, never an anchor-less
loop), `fire_budget` (the hard
per-fire deadline killed the remaining contracts — skipped loudly),
`implausible_ohlc` (vendor candle persisted verbatim + counted). ONE
request per contract per minute (NO in-minute re-poll ladder — 30
contracts × a ladder would blow the minute); a one-minute-lookback
backfill is mined from the SAME day-window body. Every unrecovered
minute is a NAMED `rest_fetch_audit` absence (round-2): skipped
selections carry `outcome=skipped` rows on the underlying's stable id
(classes `anchor_unresolved` / `anchor_stale` / `empty_selection` /
`boundary_skipped`), a fetched-but-append-failed row is
`named_gap`/`persist_failed`, and flush-lost staged minutes are
`named_gap`/`flush_failed` (the spot sweep's item-4 precedent — the
earlier `ok` row and the flush-failed row BOTH survive because `outcome`
is in the audit DEDUP key). Contract counters mirror
the spot names under the `tv_groww_contract1m_*` prefix
(`fetch_total{outcome}`, `close_to_data_ms`, `fetch_duration_ms`,
`rate_limited_total`, `boundary_skipped_total`, `task_respawn_total{reason}`,
`rows_discarded_total`, `persist_errors_total{stage}`, `ts_form_total{form}`,
`selection_truncated_total`, `selection_unresolved_total`,
`anchor_stale_total`, `token_collisions_total`,
`book_unresolved_total`, `fire_budget_exceeded_total`, `backfilled_total`).
The typed pages are `GrowwContract1mFetchDegraded` /
`GrowwContract1mFetchRecovered` (the 3-minute edge) +
`GrowwContract1mBookUnresolved` (one HIGH per day).

## §2. SPOT1M-02 — spot_1m_rest persist failed

**Severity:** High. **Auto-triage safe:** Yes (best-effort persist; the
fetch loop continues and re-appends are DEDUP-idempotent).

**Trigger:** the `spot_1m_rest` QuestDB leg failed
(`ErrorCode::Spot1m02PersistFailed`, `stage` field): the boot-time
ensure-DDL returned non-2xx / was unreachable (`stage="ensure_client_build"`
/ `stage="ensure_ddl"` — NOTE the HTTP-CLIENT-01-class consequence: the
first ILP write may auto-create the table WITHOUT DEDUP UPSERT KEYS, a
duplicate-row window until a later ensure succeeds), an ILP buffer append
was rejected (`stage="append"`), or the ILP-over-HTTP flush was refused by
the per-request server ACK (`stage="flush"` — the 2026-07-05
fire-and-forget lesson; rejects surface as `Err`, never silently). On ANY
failed flush the writer DISCARDS its pending buffer (the shadow-writer
`discard_pending` precedent, 2026-07-12 M2 fix — one server-rejected row
can never wedge the rest of the session's rows;
`tv_spot1m_rows_discarded_total` counts the drops) and the minute feeds
the SPOT1M-01 failure edge (M1 — persist failure = not-fully-OK).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `SPOT1M-02`; the `stage`
   names the failing leg. `tv_spot1m_persist_errors_total{stage}` rate
   non-zero → QuestDB ILP/HTTP degraded; run `make doctor` (cross-check
   BOOT-01/BOOT-02 if it coincides with boot).
2. The fetched values for the failed flush are DISCARDED for THAT minute
   (poisoned-buffer defense; the fetch is re-attempted next minute for
   the NEXT candle; the missed rows can be backfilled by a manual re-run
   once QuestDB recovers — DEDUP makes it safe).
3. `mcp__tickvault-logs__questdb_sql "select count(*) from spot_1m_rest
   where ts > dateadd('h', -1, now())"` — confirm rows are landing again
   after recovery (a regular trading hour = 180 rows: 60 minutes × 3).

**Honest envelope:** the table is a forensic/reference record — a persist
outage loses rows for the outage window only; it never affects tick
capture, the WS candles, or trading.

**2026-07-13 — the GROWW spot leg emits this SAME code (no new variant):**
the Groww leg's persist errors carry a **`feed = "groww"` field** on every
emit, same stage taxonomy (`append`/`flush`; ensure stages are shared —
the table DDL is one), counters under `tv_groww_spot1m_persist_errors_total{stage}`
+ `tv_groww_spot1m_rows_discarded_total` (per-feed discard series — a
Groww discard never inflates the Dhan signal). SAME table, `feed='groww'`
rows — `feed` is already in the DEDUP key, so the two feeds' rows never
collide. ADDITIONALLY (operator scope addition 2026-07-13): the NEW
**`rest_fetch_audit` per-fetch forensics table**
(`crates/storage/src/rest_fetch_audit_persistence.rs` — one row per
`(target minute, symbol, feed, leg)` fetch, success AND failure, DEDUP
`(ts, trading_date_ist, feed, leg, security_id, exchange_segment, outcome)`
— `outcome` in-key per phase-0 DEDUP rule 3 so TRANSITION rows BOTH
survive: a sweep gap row never overwrites the minute's original ladder
row; hostile round 1 item 5) is written BEST-EFFORT by the Groww leg —
AND, since 2026-07-14 (GAP-11), by the DHAN legs too: the Dhan spot leg
emits one row per (minute, SID) at every verdict/backfill/sweep/no-token
point AND per skipped boundary (`outcome=skipped`/`boundary_skipped`,
with the Groww midnight-cross date guard — review MEDIUM 1). Own-fire
`ok` rows carry the MEASURED `close_to_data_ms` from the ladder verdict;
since the same-day review HIGH fix the Dhan ladder threads a REAL
per-ladder forensics struct (`DhanLadderForensics`), so `attempts` and
`rate_limited_count` are the TRUE rung/429 counts and a terminal-429
ladder classifies `outcome=rate_limited` (the Groww last-status rule) —
a Dhan 429 storm can no longer read 0 on the scoreboard digest while
`tv_spot1m_rate_limited_total` climbs. The REMAINING named sentinels:
`final_http_status`/`fetch_latency_ms` stay 0/-1 (the Dhan
`FetchFailure` carries no status/latency fields — that narrower
follow-up stays flagged), and the budget-overrun arm honestly reads
`attempts=0` (the timed-out ladder's partial state drops with its
future). The Dhan CHAIN leg emits per (minute, underlying) with
`leg='chain_1m'` (see the §2d 2026-07-14 note). NEW COLUMN `close_to_persist_ms` (2026-07-14):
minute close → the DATA-table ILP flush-ACK instant in ms, stamped via
the hold-then-stamp pattern (`stamp_held_ok_rows` in
`spot_1m_rest_boot.rs`, shared by the Dhan spot/sweep, Groww spot fire
and Dhan chain legs) — `ok` rows are HELD until the data flush ACK and
stamped there; a FAILED flush DISCARDS the held ok rows (`outcome` is
in the DEDUP key, so a pre-flush ok append would land ALONGSIDE the
`flush_failed` named-gap rows and lie about the ok path). `-1` = not
persisted / not measured (every non-ok row, pre-2026-07-14 rows, the
Groww chain/contract legs pending their own stamping). Honest bound:
the stamp is the ILP flush ACK of an async-batched write, NOT a per-row
commit — QuestDB WAL apply can lag it. Its
ensure/append/flush failures reuse SPOT1M-02 with stages
`audit_ensure_client_build` / `audit_ensure_ddl` / `audit_append` /
`audit_flush` + `tv_rest_fetch_audit_persist_errors_total{stage}` — a
forensics write failure NEVER affects the fetch loop or the failure edge.
A minute the 15:31 sweep still cannot recover is a NAMED GAP: one
`rest_fetch_audit` row per (minute, symbol) with the DISTINCT
`outcome="named_gap"` (hostile round 1 item 6 — never a misleading
200+`error` pair; `final_http_status` = the ACTUAL last status when a
fetch happened, 0 sentinel when none) and class slugs `named_gap` /
`pre_boot` (a mid-session boot's pre-boot blind window is named
AUDIT-ONLY — §38/§9 forbid a bulk backfill fetch) / `persist_failed`
(fetched OK but the ILP APPEND failed — a persist failure, never dressed
as vendor absence; round-2 LOW) / `flush_failed` (swept minutes lost at
the ILP flush) / `no_token` — never a silent hole. DEDUP honesty
(2026-07-14 review LOW): `error_class` is NOT in the audit DEDUP key, so
two named-gap rows sharing `(minute, SID, outcome)` with different
class slugs (e.g. a fire's `persist_failed` then the sweep's
`named_gap`) UPSERT in place — the LAST-written error_class wins for
that key; the outcome-level truth is unaffected (`outcome` IS in-key).
Also since 2026-07-14 (review MEDIUM 2, cross-feed symmetry): the GROWW
spot backfill-Ok arm holds an `ok` row for the repaired minute (the Dhan
backfill-row semantics — a backfill-repaired Groww minute no longer
reads "failed, never recovered" in the digest).

**2026-07-13 — the GROWW CONTRACT leg emits SPOT1M-02 with
`leg = "contract_1m"` for the NEW `option_contract_1m_rest` table:** the
fill-model leg persists to its OWN table
(`crates/storage/src/option_contract_1m_rest_persistence.rs` — DEDUP
`(ts, security_id, exchange_segment, feed)` where `security_id` is the
contract's Groww exchange_token and `exchange_segment` is
`NSE_FNO`/`BSE_FNO`; the float `strike` is deliberately NOT in-key;
retention registered in the partition manager's DAY sweep list). Persist
failures reuse `SPOT1M-02` with `feed = "groww"` + `leg = "contract_1m"`,
stages `ensure_client_build` / `ensure_ddl` / `append` / `flush` /
`audit_append` / `audit_flush`, counters
`tv_groww_contract1m_persist_errors_total{stage}` +
`tv_groww_contract1m_rows_discarded_total` (discard-pending on any failed
flush — the poisoned-buffer defense; rows are DEDUP-idempotent
re-fetchable). One `rest_fetch_audit` row per (minute, contract) with
`leg='contract_1m'` — `security_id` = the exchange_token, `symbol` = the
UNDERLYING's plain symbol (the full contract identity lives in the data
table's `groww_symbol` column).

## §2b. CHAIN-01 — option-chain entitlement absent (pipeline down for the day)

**Severity:** High. **Auto-triage safe:** NO (severity-independent
override — restoring the Option Chain Data-API entitlement is an
operator/broker ACCOUNT decision, never a code fix; the FUTIDX-02
precedent).

**Trigger:** Dhan rejected an expirylist / option-chain call with the
entitlement class (`ErrorCode::Chain01EntitlementAbsent`): the body names
`DH-902` or Data-API `806`, or a 401/403 whose body names the missing
SUBSCRIPTION (a bare 401 is a token-class transient — the renewal
machinery owns it, never this code). Two stages: `stage="warmup"` (the
day-start expirylist doubles as the probe) and `stage="mid_session"` (the
entitlement was revoked intra-day). Both fire ONCE per day (edge — the
pipeline stops immediately, so no per-minute 401 storm is possible by
construction), page the typed HIGH `ChainEntitlementAbsent` Telegram
event (with `pipeline_enabled: true`), and the chain pipeline stays DOWN
for the day (the supervisor deliberately does NOT respawn a
disabled-for-the-day stop). The probe-only path (`enabled = false`)
reports the same verdict as an INFO Telegram — a report the operator
asked for, never a page.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CHAIN-01`; the payload
   carries the bounded secret-redacted broker body (status + redacted URL
   + ≤300 chars).
2. Verify the account's Data-API plan on the Dhan portal — the Option
   Chain API needs its own entitlement (absent on this account as of June
   2026). Either buy/renew it, or flip `[option_chain_1m].enabled = false`
   so the daily page stops.
3. After the entitlement is restored, the next trading-day boot warms up
   and runs automatically — no code change.

**Honest envelope:** the classification is derived from Dhan's error body
wording; a Dhan-side rewording could degrade an entitlement reject to the
transient CHAIN-02 class — LOUDER (the 3-minute escalation edge still
pages), never silent. The 401/403 wording arm additionally requires the
DHAN ERROR-JSON SHAPE (`errorCode`/`errorType` field present —
`body_has_dhan_error_shape`), so a gateway/WAF HTML block page mentioning
"subscription" classifies TRANSIENT and can never kill the day
(hostile-review M2). Residual false-positive direction: a genuine
Dhan-shaped 403 naming "subscription" for a non-entitlement reason still
classifies absent — bounded to ONE HIGH page, day-scoped, the WS feed
untouched; triage step 2 (the Dhan portal check) disambiguates.

**2026-07-13 — the GROWW chain leg NEVER emits CHAIN-01:** Groww's chain
endpoint has no separate Data-API entitlement question (the shared-minter
access token is the only gate) and no expirylist endpoint (expiries come
from the already-ingested daily instruments CSV), so a Groww auth /
transport reject is a CHAIN-02 transient (`feed = "groww"`), never a
day-killing CHAIN-01.

## §2c. CHAIN-02 — per-minute option-chain fetch degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; the next minute boundary re-attempts automatically).

**Trigger:** one of (`ErrorCode::Chain02FetchDegraded`, distinguished by
the `stage` field — the SPOT1M-01 taxonomy applied to the chain leg):

1. `stage="minute_failed"` — coalesced ONCE per fired minute when one or
   more of the 3 underlyings failed: transport error, non-2xx (non-
   entitlement), the per-underlying hard budget
   (`CHAIN_1M_UNDERLYING_BUDGET_SECS`, 20 s) overran, the body was not a
   parseable option chain, no token at fire time, or a 200 whose chain
   carried ZERO strikes (`outcome="empty"` — counted, included in the
   failure edge, never silent per audit Rule 11). Sub-edge: log-only.
2. `stage="escalation"` — the EDGE: 3 consecutive fully-failed minutes
   ("fully failed" = no underlying succeeded OR the persist leg failed —
   the spot M1 persist-gated rule). Fires ONCE per episode + the typed
   HIGH `ChainFetchDegraded` Telegram event; re-armed only after a minute
   where fetch AND persist both succeed (recovery = one Info
   `ChainFetchRecovered`).
3. `stage="client_build"` / `stage="task_respawn"` — the long-lived HTTP
   client could not be built (HTTP-CLIENT-01 class) or the scheduler task
   died and the supervisor respawned it
   (`tv_chain1m_task_respawn_total{reason}`).
4. `stage="boundary_skipped"` — minute boundaries elapsed UNFETCHED (fire
   overrun / suspend / clock step). Counted by
   `tv_chain1m_boundary_skipped_total`, coalesced to ONE coded log, each
   missed minute FEEDS the failure edge.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CHAIN-02`; the payload
   carries `stage`, the failing minute (IST), per-underlying outcomes and
   a bounded secret-redacted sample failure.
2. Cross-check the SPOT leg (SPOT1M-01) and the REST canary
   (REST-CANARY-01): spot healthy + chain failing = an option-chain-API-
   surface problem (entitlement wobble, chain-specific gateway); both
   failing = REST/token — the AUTH-GAP runbooks.
3. `tv_chain1m_fetch_total{outcome="ok"|"empty"|"error"|"entitlement"}`
   rates name the dominant class; the `tv_chain1m_close_to_data_ms`
   histogram is the live sequencing-latency measurement (minute close →
   chain retrieved, normally ~1.5–4 s: spot completion + one chain
   round-trip).

**Honest envelope:** one request per underlying per minute, NO in-minute
re-poll ladder (the chain is a live snapshot, not a just-sealed candle) —
a failed minute is a counted, coded, visible gap; re-appends UPSERT in
place so a later manual re-run backfills safely.

**2026-07-13 — the GROWW chain leg emits this SAME code (no new
variant):** the Groww per-minute option-chain leg
(`crates/app/src/groww_option_chain_1m_boot.rs`, plan
`.claude/plans/active-plan-groww-rest-1m.md` PR-3) reuses `CHAIN-02` with
the SAME stage taxonomy, distinguished by a **`feed = "groww"` field on
every emit** (the Dhan sites stay field-less — grep `feed="groww"` to
split). Groww-specific additions: `stage="probe"` (the boot-time
probe-only path — one bounded chain GET, verdict via Info Telegram,
persists nothing), `stage="expiry_unresolved"` (the daily Groww
instruments CSV carried no FNO CE/PE rows ≥ today for an underlying —
that underlying degrades for the day, coded + counted by
`tv_groww_chain1m_expiry_unresolved_total` + the typed HIGH
`GrowwChain1mExpiryUnresolved` Telegram; NEVER guessed), and
`stage="strikes_truncated"` (a hostile/oversized chain body hit the
strike cap — truncated + counted, never unbounded). `stage="token_read"`
lives on the shared token cache (`tv_groww_chain1m_token_read_failed_total`
— re-read paced ≥60 s, NEVER minted). Sequencing mirrors the Dhan leg:
the chain fires on the spot leg's watch signal after every spot fire,
bounded by the ~2.5 s fallback timer; one request per underlying per
minute, sequential, with a defensive 1 s min-gap (Groww documents no
chain-specific rate rule). Groww counters mirror the Dhan names under the
`tv_groww_chain1m_*` prefix (`fetch_total{outcome}`, `close_to_data_ms`,
`fetch_duration_ms`, `strikes_per_chain`, `legs_per_chain`,
`payload_bytes`, `rate_limited_total`, `boundary_skipped_total`,
`underlying_budget_exceeded_total`, `invalid_strikes_total`,
`task_respawn_total{reason}`). The typed pages are the Groww-specific
`GrowwChain1mFetchDegraded` / `GrowwChain1mFetchRecovered` Telegram
events (same 3-minute persist-gated edge). Forensics: one
`rest_fetch_audit` row per (minute, underlying) with `leg='chain_1m'`,
`attempts=1` (no re-poll ladder — live snapshot semantics). Gate note
(2026-07-13): the Groww chain leg shipped base.toml DEFAULT-OFF
(probe-only); the first live probe PASSED the same night (build
`eeca0ec`, ~11:47 PM IST) and `[groww_option_chain_1m].enabled` flipped
to `true` in base.toml — dated record in
`groww-second-feed-scope-2026-06-19.md` §38.6; the serde DEFAULT stays
OFF (fail-safe) and `probe_and_report` stays `true` (inert while
enabled; the rollback canary).

**2026-07-14 — the GROWW leg gains a per-underlying not-served paging
edge (`stage="underlying_not_served"`):** the motivating incident — on
expiry day 2026-07-14 Groww stopped serving NIFTY's same-day-expiring
chain at 14:54 IST (2xx, zero strikes, `outcome=empty`) while BANKNIFTY
+ SENSEX kept working (`ok=2 empty=1` per minute, ALL afternoon), and
NOTHING paged: the `stage="escalation"` edge arms only on FULLY-failed
minutes (ok == 0), so a single-underlying vendor cutoff was invisible.
The new arm mirrors the spot leg's `sid_not_served` detector (§1 item
5): a minute COUNTS toward an underlying's streak only when that
underlying's chain came back empty/failed (FETCH-level — Empty AND
error-class count the same; persist failures stay the escalation edge's
M1 business) while ≥1 OTHER underlying was OK in the SAME minute; a
global-failure minute (zero OK) neither counts nor resets — so within
the FETCH-failure class the two edges are mutually exclusive per minute
(the escalation edge needs ok == 0, this edge needs ≥1 OK). HONEST
OVERLAP: a persist-failed minute with ok ≥ 1 can legitimately count
toward BOTH edges (the M1 persist gate makes the escalation edge count
it fully-failed while an empty sibling counts here) — two DISTINCT
signals: persistence broken + vendor not serving one underlying. An
auth-aborted fire (401 short-circuit — a global token condition, even
after an earlier underlying succeeded) is a tracker HOLD: neither
counts nor resets. At
`GROWW_CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD` (10) consecutive
counted minutes: ONE `error!(code = CHAIN-02,
stage = "underlying_not_served", feed = "groww", underlying,
consecutive_minutes)` + ONE typed HIGH `GrowwChain1mUnderlyingNotServed`
Telegram page per underlying per episode (edge-latched, Rule 4;
re-armed only by that underlying's own recovery — falling edge = one
Info `GrowwChain1mUnderlyingServedRecovered`). Counter:
`tv_groww_chain1m_underlying_not_served_total{underlying}` (3 static
label values — the pinned plain symbols), one increment per counted
minute. The typed HIGH Telegram event IS the page — CHAIN-02 remains
log-sink-only per §3. Streak state is per scheduler run (per trading
day; a mid-day task respawn restarts it — the FailureEdge envelope).
Source: `crates/app/src/groww_option_chain_1m_boot.rs`
(`UnderlyingServedTracker` / `record_groww_chain_underlying_verdicts`).

## §2d. CHAIN-03 — option_chain_1m persist failed

**Severity:** High. **Auto-triage safe:** Yes (best-effort persist; the
fetch loop continues and re-appends are DEDUP-idempotent).

**Trigger:** the `option_chain_1m` QuestDB leg failed
(`ErrorCode::Chain03PersistFailed`, `stage` field): the boot-time
ensure-DDL returned non-2xx / was unreachable (`stage="ensure_client_build"`
/ `stage="ensure_ddl"` — NOTE the HTTP-CLIENT-01-class consequence: the
first ILP write may auto-create the table WITHOUT DEDUP UPSERT KEYS, a
duplicate-row window until a later ensure succeeds), an ILP buffer append
was rejected (`stage="append"` — the underlying's remaining legs are
skipped for the minute), or the ILP-over-HTTP flush was refused by the
per-request server ACK (`stage="flush"`). On ANY failed flush the writer
DISCARDS its pending buffer (the shadow-writer `discard_pending`
precedent — one server-rejected row can never wedge the session;
`tv_chain1m_rows_discarded_total` counts the drops) and the minute feeds
the CHAIN-02 failure edge.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CHAIN-03`; the `stage`
   names the failing leg. `tv_chain1m_persist_errors_total{stage}` rate
   non-zero → QuestDB ILP/HTTP degraded; run `make doctor` (cross-check
   BOOT-01/BOOT-02, and SPOT1M-02 — the two writers share the QuestDB
   target, so they degrade together).
2. `mcp__tickvault-logs__questdb_sql "select count(*) from option_chain_1m
   where ts > dateadd('h', -1, now())"` — confirm rows are landing again
   after recovery (a healthy trading hour ≈ tens of thousands of rows:
   ~60 minutes × up to ~2-3K legs).
3. Disk pressure is a plausible root cause for THIS table specifically
   (~70 MB/day — an order of magnitude above the other DAY tables); check
   `df -h /data` + RESOURCE-03 and see the retention follow-up note in
   `option_chain_1m_persistence.rs`.

**Day-1 enabled-boot operator checklist (2026-07-13, review M2 — the
DOUBLE-strike/TIMESTAMP-expiry DEDUP key is UNVERIFIED-LIVE):** on the
FIRST boot with `[option_chain_1m].enabled = true`:
1. Watch boot logs for `CHAIN-03` with `stage="ensure_client_build"` /
   `stage="ensure_ddl"` — a rejected/skipped DEDUP DDL means the first ILP
   write may auto-create `option_chain_1m` WITHOUT DEDUP UPSERT KEYS: a
   SILENT duplicate-row window until a later boot's ensure succeeds.
2. Verify DEDUP actually engaged:
   `mcp__tickvault-logs__questdb_sql "select * from wal_tables() where name = 'option_chain_1m'"`
   (table present + not suspended), and after the first enabled hour run
   the duplicate spot check —
   `mcp__tickvault-logs__questdb_sql "select ts, security_id, strike, leg, count(*) c from option_chain_1m group by ts, security_id, strike, leg order by c desc limit 5"`
   (adjust column names to the live schema if they differ) — every `c`
   MUST be 1. Any `c > 1` = DEDUP did not engage; fix the DDL (re-run the
   ensure via a restart) and manually dedup the window before trusting
   the day's rows.

**Honest envelope:** the table is a forensic/reference capture — a persist
outage loses chain rows for the outage window only; it never affects tick
capture, the WS candles, or trading. **Partial-persist wrinkle
(hostile-review L4, documented — no code change):** an append failure
mid-underlying skips that underlying's REMAINING legs while
earlier-appended legs (including other underlyings already in the buffer)
may still flush — so a minute can be PARTIALLY present in the table while
the failure edge honestly counts it fully-failed (persist-gated). A later
re-run/backfill of the same minute UPSERTs in place (DEDUP-idempotent)
and heals the partial rows.

**2026-07-13 — the GROWW chain leg emits this SAME code (no new
variant):** the Groww leg writes the SAME `option_chain_1m` table with
`feed='groww'` rows (`feed` is already in the DEDUP key — the two feeds'
rows never collide), same stage taxonomy (`append`/`flush`; the
ensure-DDL stages are shared — one table, one DDL), with a **`feed =
"groww"` field on every emit** and counters under
`tv_groww_chain1m_persist_errors_total{stage}` +
`tv_groww_chain1m_rows_discarded_total`. Two columns added 2026-07-13 via
ALTER-ADD self-heal for the Groww leg: `rho` (Groww supplies it; Dhan
does not) and `close_to_data_ms` (per-row latency stamp — the ONLY
freshness signal, since Groww's chain response carries NO timestamp).
Dhan rows leave both NULL (the Dhan `option_chain_1m` emit path is
untouched — `rho`/`close_to_data_ms` are Groww-leg columns). The Groww
leg's `rest_fetch_audit` forensics failures reuse the SPOT1M-02 stage
names `audit_append` / `audit_flush` but are coded CHAIN-03 with
`leg='chain_1m'` context — a forensics write failure NEVER affects the
fetch loop or the failure edge.

**2026-07-14 — the DHAN chain leg now emits `rest_fetch_audit` rows too
(GAP-11):** one row per (fired minute, underlying), `feed='dhan'`,
`leg='chain_1m'`, `attempts=1` (live-snapshot semantics — no re-poll
ladder), keyed on the underlying's SID/plain symbol (the Groww chain
precedent): `ok` (real `close_to_data_ms`, held-until-flush-ACK
`close_to_persist_ms` per the §2 hold-then-stamp contract) / `empty`
(`empty_chain`, 200) / `error` (`error` or `entitlement` class; the
Dhan chain ladder surfaces no HTTP status — 0 sentinel, honest) /
`no_token` / `skipped` (`boundary_skipped`, per missed boundary, with
the Groww midnight-cross date guard) / `named_gap`
(`persist_failed`/`flush_failed` for fetched-but-lost minutes). Its
forensics-write failures are coded CHAIN-03 `audit_append`/`audit_flush`
(Dhan emit sites stay field-less — grep-split by `feed="groww"`).
Deliberate edge-accounting change riding the midnight guard (review
MEDIUM 3, same day): on a midnight-crossing wake the guard now also
skips the per-boundary `edge.record_minute(true)` calls that pre-GAP-11
ran unconditionally — the trading day those boundaries belonged to is
over, so an escalation page for yesterday's tail would be noise.

## §2e. CHAIN-04 — day-start expirylist warmup failed (pipeline down for the day)

**Severity:** High. **Auto-triage safe:** Yes (the next trading-day boot
re-warms automatically; nothing to fix mid-day unless the REST surface
itself is down — which other codes own).

**Trigger:** the day-start `POST /v2/optionchain/expirylist` warmup
failed after bounded retries — 3 attempts per underlying with 3 s / 6 s
backoffs (`CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS`; each ≥ the 3 s
unique-request window, so a retry can never earn the rate-limit reject it
retries) — or every listed expiry was already past (a stale-data
anomaly) (`ErrorCode::Chain04ExpirylistFailed`, `stage="warmup"`). Expiry
dates come ONLY from the API (option-chain.md rule 9) — the pipeline
NEVER guesses one, so it degrades to DISABLED-FOR-THE-DAY: one coded
error + `tv_chain1m_expirylist_failed_total` + ONE typed HIGH
`ChainExpirylistFailed` Telegram page, and the supervisor exits without
respawn.

**Bounded respawn-retry arms (NOT down-for-the-day — hostile-review M1
truth-sync):** `stage="warmup_no_token"` (no access token at warmup time)
logs the coded error and returns WITHOUT disabling the day — the
supervisor respawns after the 30 s backoff and the warmup retries until
the token machinery delivers one (no Telegram, no expirylist counter —
the AUTH-GAP runbooks own the token page). The HTTP-client build failure
arm behaves the same way but is coded CHAIN-02 (`stage="client_build"` —
see §2c item 3). The probe-only path logs `stage="probe_inconclusive"` /
`stage="probe_client_build"` / `stage="probe_no_token"` /
`stage="probe_task_exit"` (probe task died in an unwind build) on a
transient probe failure — log-sink only, no verdict that day (tomorrow's
boot re-probes).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CHAIN-04`; the payload
   names the first failing underlying + the bounded secret-redacted
   failure.
2. Cross-check REST-CANARY-01 + the WS feed: a healthy REST surface with
   ONLY the expirylist failing points at the option-chain API
   specifically (possibly the entitlement wobbling short of a clean
   DH-902 — see CHAIN-01).
3. No mid-day recovery path by design (the expiry set is a day-start
   decision); the next trading-day boot re-warms. If the chain data
   matters TODAY, restart the app once the REST surface is healthy — the
   warmup re-runs at boot.

**Honest envelope:** an aggressive mid-day auto-retry loop against a
failing expirylist would be a reject storm for zero benefit; one loud
page + a clean daily boundary is the honest degrade.

**2026-07-13 — the GROWW chain leg NEVER emits CHAIN-04:** there is no
Groww expirylist endpoint — the CURRENT expiry per underlying is resolved
from the already-ingested daily Groww instruments CSV (nearest ≥ today,
never-roll). A master that carries no usable FNO rows for an underlying
degrades THAT underlying via CHAIN-02 `stage="expiry_unresolved"`
(`feed = "groww"`), never a whole-pipeline CHAIN-04 day-kill.

## §2f. 2026-07-14 — shared self-tuning Dhan Data-API rate limiter (3→2 rps) + spot retry-shaping + fetch-mode flag

**Operator directive (2026-07-14, relayed verbatim via the coordinator
session):** pace Dhan to 3 requests/sec (tunable DOWN to 2), spread overflow
into the next second(s), route the option-chain API through the SAME
limiter, with incremental/decremental self-tuning — *"if it accepts max 3
or 2, stick to that and split it up"*. Dhan-ONLY; Groww untouched.

**HONESTY (read first):** on 2026-07-14 the spot leg was **0/980 (0%)** with
**~244 wasted 429s** (the ladder re-fired ~20 req/min against all-empty
responses; the first two rungs are 0.7s apart → ~8 requests in a rolling
second at the fire instant) while the chain leg was **735/735 (100%)**. THE
LIMITER ELIMINATES THE 429 WASTE AND MAKES US A GOOD API CITIZEN — IT DOES
NOT MAKE DHAN SERVE SAME-DAY CANDLES. The 429s are a symptom of hammering
empties, not the root cause of the 0%. The per-minute-vs-batch architecture
question is decided by the separate ~15:40 IST sweep-discriminator verdict;
this change makes that a CONFIG MODE, not a rewrite.

**The limiter** (`crates/app/src/dhan_data_api_limiter.rs`): ONE
process-wide async token-bucket gate every per-minute Dhan Data-API REST
fire passes through — spot-1m fires + ladder re-polls + the 15:33:30 sweep
+ the #1524 diagnostic probes + the option-chain fires + expirylist
warmup/probe. Routing choke points: `spot_1m_rest_boot::spot_1m_fetch_once`
and `option_chain_1m_boot::chain_fetch_once` (every enumerated caller
funnels through them; ratchet
`crates/app/tests/dhan_data_api_limiter_wiring_guard.rs`). `acquire().await`
semantics — overflow spills into later seconds, nothing errors or drops.
The chain's 1-unique-per-3s per-underlying min-gap stays layered on top,
unchanged. Config: `[dhan_data_api] target_rps = 3` (serde default 3; legal
range 2..=4 rejected at boot by `ApplicationConfig::validate`).

**Self-tuning levels:** pre-built governor cells per level (2..=4) swapped
via `ArcSwap` (quotas are fixed at construction); the pure `RpsTuner` FSM
is fed by REAL StatusCode-429 observations — ≥3 429s in a rolling 2-minute
window → ONE step DOWN to the 2 rps floor (the operator's literal "step
DOWN to 2"; at the default target 3 that is also exactly one level); 10
clean minutes → ONE step back UP one level toward the config cap. Every
transition is edge-logged once (`error!` down / `info!` up — codeless
degrade lines, this section is their runbook) + `tv_dhan_data_api_rps`
gauge + `tv_dhan_data_api_tuner_transitions_total{direction}`.

**Spot retry-shaping:** (a) STALE-WATERMARK CUTOFF — a ladder attempt whose
parsed day payload carries the SAME last-candle watermark as the previous
attempt's (including two consecutive zero-row payloads) STOPS the ladder
for that minute (`tv_spot1m_ladder_watermark_cutoff_total`; re-polling
cannot outrun a serving delay per the #1524 serving-lag data — the honest
trade-off is the 1.5–6s marginal-appearance window, repaired by the
backfill + sweep). (b) ADAPTIVE DEGRADE — after 5 consecutive no-data
minutes (zero SIDs served), single-attempt-per-minute until ANY success
re-arms the full ladder (`tv_spot1m_ladder_degraded` gauge, one warn!/info!
per transition). The pre-existing slot-jitter is KEPT as a harmless
schedule de-sync — the limiter is the pacing authority now.

**Fetch-mode flag:** `[spot_1m_rest] fetch_mode = "per_minute" |
"batch_catchup"` (serde default per_minute; `batch_interval_minutes`
default 5, validated 1..=60). Batch mode = a sweep-style catch-up every K
minutes through the SAME limiter, reusing the shared
`sweep_sids_above_watermark` helper (also used by the 15:33:30 sweep) —
`tv_spot1m_batch_cycles_total{outcome}` + the same SPOT1M-01 FailureEdge
escalation events per cycle. Default stays per_minute pending the operator
ruling.

**Triage:** `tv_dhan_data_api_rps` pinned at 2 all session = Dhan keeps
rejecting the 3 rps pace (cross-check `tv_spot1m_rate_limited_total` +
`tv_chain1m_fetch_total{outcome="error"}`); a step-down/step-up flap cycle
is bounded to ≤1 per ~12 min by the 10-minute clean-streak requirement.
Scope boundary (honest): the boot-time prev-day fetch and the 15:31 bulk
cross-verify keep their own pacing cells (disjoint windows; not in the
operator's enumerated scope — unifying them is a flagged follow-up).

## §3. Delivery boundary (honest — no false-OK)

**2026-07-14 UPDATE (REST-audit GAP-03 —
`docs/audits/2026-07-14-rest-pipeline-adversarial-audit.md`): the flagged
CloudWatch follow-up below is now PARTIALLY LANDED.** Four SCOPED
`error_code_alerts` entries exist in
`deploy/aws/terraform/error-code-alarms.tf` (+ the doc paging list +
`error_code_paging_filter_drift_guard.rs` pattern-shape extension for one
extra `$.field` clause, all in lockstep):

| Entry | Filter scope | Why scoped |
|---|---|---|
| `spot1m-01-escalation` | `$.stage = "escalation"` only | the per-minute `minute_failed`/`boundary_skipped` lines fire every failed minute — a plain code filter would over-page vs the designed 3-minute edge; covers the Dhan spot + Groww spot + Groww contract legs (same code) |
| `chain-02-escalation` | `$.stage = "escalation"` only | same rationale, both feeds' chain legs |
| `chain-01` | plain coded filter | both stages (warmup + mid_session) are once-per-episode page-worthy; the probe-only path never emits CHAIN-01 at ERROR |
| `chain-04-warmup` | `$.stage = "warmup"` only | the probe_* / warmup_no_token stages are log-only-by-design transient/respawn arms (warmup_no_token repeats every ~30s until a token exists) |

**Still log-sink-only (deliberate):** the persist codes SPOT1M-02 +
CHAIN-03 have no direct filter — every persist failure feeds the
persist-gated 3-minute escalation edge (the M1 rule), so a sustained
persist outage still reaches the `spot1m-01-escalation` /
`chain-02-escalation` pages; a direct filter on the per-minute persist
lines would over-page. The Groww-leg CHAIN-04-class degrades
(`expiry_unresolved` etc.) remain CHAIN-02-stage territory per §2c.

The typed HIGH Telegram event at the 3-minute escalation edge (+ the Info
recovery event) remains the primary operator page — and, for the chain
half, the once-per-day `ChainEntitlementAbsent` / `ChainExpirylistFailed`
HIGH pages + the probe-verdict Infos; the coded `error!` lines are the
forensic WHY. The CloudWatch entries are the BACKSTOP leg that survives a
dead app notifier / dropped Telegram (the audit's GAP-05 class — see also
`telegram-drop-alarm.tf`).

**Contract-leg honest envelope (2026-07-13, PR-4):** UNVERIFIED-LIVE —
FNO per-contract 1m candle availability latency for the just-sealed
minute (the `tv_groww_contract1m_close_to_data_ms` histogram is the
probe); whether Groww gap-fills or OMITS zero-trade thin-strike minutes
(an absent contract minute is counted `outcome="empty"`, reported, never
fabricated); the live per-contract response shape beyond the
production-grounded `[ts, o, h, l, c, volume, oi]` tuple. DELIBERATELY NO
15:31 post-session sweep for contracts: the selection is minute-scoped
(the ATM window moves with the chain anchor), so "which contracts belong
to minute M" is only knowable AT minute M — an unrecovered contract
minute is a NAMED absence via its `rest_fetch_audit` row, never a silent
hole. The one-minute-lookback backfill (mined from the same day-window
body) is the only cross-minute repair.

**Decision-freshness gate (2026-07-13 — mirrors
`groww-second-feed-scope-2026-06-19.md` §38.8):** backfill/sweep-repaired
rows in `spot_1m_rest` / `option_chain_1m` / `option_contract_1m_rest`
are RECORD-COMPLETENESS data (backtest parity, cross-verify, audit) —
NEVER trading-decision inputs. Any future strategy consumer MUST fail
closed on staleness (a row older than a configured freshness threshold ⇒
no trade that minute). Stale rows are mechanically distinguishable TODAY:
`close_to_data_ms ≥ 60000` on backfilled/swept rows vs ~1-2 s own-fire,
and the `rest_fetch_audit` outcome names the recovery path. No strategy
consumer exists (the §28 boundary); building one needs its own operator
scope.

**2026-07-13 update — the daily Quote-2 digest is LIVE (Groww REST plan
PR-5):** the 15:45 IST dual-feed scorecard now carries one plain-English
"Official minute candles — how fast after each minute closed" line per
(feed, leg), aggregated from the day's `rest_fetch_audit` rows (+ the
`spot_1m_rest` latency-fallback column for the Dhan spot leg — the
fallback is HISTORICAL/defensive since 2026-07-14: the Dhan spot + chain
forensics rows are LIVE per the §2/§2d GAP-11 notes, so the digest's
primary source now covers all four feed/leg pairs) — prompt-pull p50/p99/max
seconds-after-close, ok/failed counts, rate-limit hits, late recoveries
and never-recovered gaps, all MEASURED, `-1` sentinels rendering "not
measured yet". Degrade stages `rest_leg_*` live under SCOREBOARD-01; full
contract in `dual-feed-scoreboard-error-codes.md` §2b. The per-fire
histograms + typed pages above are UNCHANGED — the digest is the daily
plain-English summary the §9.3 mandate demanded, not a new pager. The contract leg's `rest_fetch_audit` rows (PR-4,
`leg='contract_1m'`) feed the SAME digest line automatically — no
digest-side change was needed.

## §4. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Spot1m0*` or `Chain0*` variant)
- `crates/app/src/spot_1m_rest_boot.rs`
- `crates/app/src/option_chain_1m_boot.rs`
- `crates/app/src/groww_spot_1m_boot.rs` (the 2026-07-13 Groww leg)
- `crates/app/src/groww_option_chain_1m_boot.rs` (the 2026-07-13 Groww
  chain leg)
- `crates/app/src/groww_contract_1m_boot.rs` (the 2026-07-13 Groww
  contract leg — PR-4, the fill-model leg)
- `crates/storage/src/spot_1m_rest_persistence.rs`
- `crates/storage/src/option_chain_1m_persistence.rs`
- `crates/storage/src/option_contract_1m_rest_persistence.rs` (the
  2026-07-13 PR-4 fill-model table)
- `crates/storage/src/rest_fetch_audit_persistence.rs` (the 2026-07-13
  per-fetch forensics table)
- `crates/common/src/config.rs` (`Spot1mRestConfig` / `OptionChain1mConfig`
  / `GrowwSpot1mConfig` / `GrowwOptionChain1mConfig` / `GrowwContract1mConfig`
  / `DhanDataApiConfig` / `SpotFetchMode`)
- `crates/app/src/dhan_data_api_limiter.rs` (the 2026-07-14 shared
  self-tuning limiter — §2f)
- Any file containing `SPOT1M-01`, `SPOT1M-02`, `Spot1m01FetchDegraded`,
  `Spot1m02PersistFailed`, `spot_1m_rest`, `SPOT_1M_REST_INDICES`,
  `tv_spot1m_fetch_total`, `tv_spot1m_serving_lag_ms`, `EmptyClass`,
  `dhan_data_api_limiter`, `tv_dhan_data_api_rps`, `RpsTuner`,
  `ladder_watermark_repeated`, `LadderDegrade`, `batch_catchup`,
  `CHAIN-01`, `CHAIN-02`, `CHAIN-03`, `CHAIN-04`,
  `Chain01EntitlementAbsent`, `Chain02FetchDegraded`,
  `Chain03PersistFailed`, `Chain04ExpirylistFailed`, `option_chain_1m`,
  `tv_chain1m_fetch_total`, `GROWW_SPOT_1M_SYMBOLS`, `rest_fetch_audit`,
  `tv_groww_spot1m_fetch_total`, `GROWW_CHAIN_1M_UNDERLYINGS`,
  `tv_groww_chain1m_fetch_total`, `option_contract_1m_rest`,
  `GROWW_CONTRACT_1M_MAX_PER_MINUTE`, or `tv_groww_contract1m_fetch_total`
