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
   `tv_spot1m_rate_limited_total`, never retried past the ladder), no
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
`(ts, trading_date_ist, feed, leg, security_id, exchange_segment)`) is
written BEST-EFFORT by the Groww leg (the Dhan leg's emit sites are a fast
FOLLOW-UP after #1499 merges); its ensure/append/flush failures reuse
SPOT1M-02 with stages `audit_ensure_client_build` / `audit_ensure_ddl` /
`audit_append` / `audit_flush` + `tv_rest_fetch_audit_persist_errors_total{stage}`
— a forensics write failure NEVER affects the fetch loop or the failure
edge. A minute the 15:31 sweep still cannot recover is a NAMED GAP: one
`rest_fetch_audit` row (`error_class="named_gap"` class slugs) per
(minute, symbol) — never a silent hole.

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

## §3. Delivery boundary (honest — no false-OK)

All six codes (SPOT1M-01/02 + CHAIN-01..04) are **log-sink-only today**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list (the paging drift guard pins
those surfaces untouched). The operator page for a failing pipeline is the
typed HIGH Telegram event at the 3-minute escalation edge (+ the Info
recovery event) — and, for the chain half, the once-per-day
`ChainEntitlementAbsent` / `ChainExpirylistFailed` HIGH pages + the
probe-verdict Infos; the coded `error!` lines are the forensic WHY. Adding a
CloudWatch filter+alarm is a flagged follow-up (one map entry + the doc
paragraph + a cost note, per the FEED-REJECT-01 / SCOREBOARD-01 precedent).

## §4. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Spot1m0*` or `Chain0*` variant)
- `crates/app/src/spot_1m_rest_boot.rs`
- `crates/app/src/option_chain_1m_boot.rs`
- `crates/app/src/groww_spot_1m_boot.rs` (the 2026-07-13 Groww leg)
- `crates/storage/src/spot_1m_rest_persistence.rs`
- `crates/storage/src/option_chain_1m_persistence.rs`
- `crates/storage/src/rest_fetch_audit_persistence.rs` (the 2026-07-13
  per-fetch forensics table)
- `crates/common/src/config.rs` (`Spot1mRestConfig` / `OptionChain1mConfig`
  / `GrowwSpot1mConfig`)
- Any file containing `SPOT1M-01`, `SPOT1M-02`, `Spot1m01FetchDegraded`,
  `Spot1m02PersistFailed`, `spot_1m_rest`, `SPOT_1M_REST_INDICES`,
  `tv_spot1m_fetch_total`, `CHAIN-01`, `CHAIN-02`, `CHAIN-03`, `CHAIN-04`,
  `Chain01EntitlementAbsent`, `Chain02FetchDegraded`,
  `Chain03PersistFailed`, `Chain04ExpirylistFailed`, `option_chain_1m`,
  `tv_chain1m_fetch_total`, `GROWW_SPOT_1M_SYMBOLS`, `rest_fetch_audit`, or
  `tv_groww_spot1m_fetch_total`
