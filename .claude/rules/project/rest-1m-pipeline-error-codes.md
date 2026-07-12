# Per-Minute REST 1m Pipeline — Error Codes (SPOT1M-01 / SPOT1M-02)

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
> requires this file to mention every `Spot1m0*` variant verbatim —
> `SPOT1M-01` and `SPOT1M-02` appear below.
> **Future codes:** the option-chain half of this pipeline (PR-3) will
> APPEND its `CHAIN-*` code sections to THIS file — one runbook for the
> whole per-minute REST pipeline family.

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

## §3. Delivery boundary (honest — no false-OK)

Both codes are **log-sink-only today**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list (the paging drift guard pins
those surfaces untouched). The operator page for a failing pipeline is the
typed HIGH Telegram event at the 3-minute escalation edge (+ the Info
recovery event); the coded `error!` lines are the forensic WHY. Adding a
CloudWatch filter+alarm is a flagged follow-up (one map entry + the doc
paragraph + a cost note, per the FEED-REJECT-01 / SCOREBOARD-01 precedent).

## §4. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Spot1m0*` variant)
- `crates/app/src/spot_1m_rest_boot.rs`
- `crates/storage/src/spot_1m_rest_persistence.rs`
- `crates/common/src/config.rs` (`Spot1mRestConfig`)
- Any file containing `SPOT1M-01`, `SPOT1M-02`, `Spot1m01FetchDegraded`,
  `Spot1m02PersistFailed`, `spot_1m_rest`, `SPOT_1M_REST_INDICES`, or
  `tv_spot1m_fetch_total`
