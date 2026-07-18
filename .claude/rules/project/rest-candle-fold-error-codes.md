---
paths:
  - "crates/app/src/rest_candle_fold.rs"
  - "crates/common/src/error_code.rs"
  - "crates/common/src/config.rs"
  - "config/base.toml"
  - "crates/app/src/spot_1m_rest_boot.rs"
  - "crates/app/src/groww_spot_1m_boot.rs"
---

# REST-Era Multi-TF Candle Derivation — Error Codes (FOLD-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `live-feed-purity.md` (rule 10 carries the dated 2026-07-16 candles_1d
> edit; rules 1-6 — no synthesized ticks — stand untouched) >
> `rest-1m-pipeline-error-codes.md` (the `spot_1m_rest` source legs) >
> this file.
> **Operator directive (2026-07-16, verbatim):** *"why the fuck remaining
> candles 1m till 1day is not yet generated and populated — resolve these"*
> + *"for only spots we will have minimum one month data because anyhow
> based on underlying spots alone only trading decision will be entered or
> exited — but option only for the current day"* + *"everything should be
> always available in our own questdb right — our entire one month should
> be stored and fetched from questdb even before premarket"*.
> **Companion code:** `crates/app/src/rest_candle_fold.rs` (pure fold core
> + confirmed-bar handoff + boot catch-up + current-day day-map refold +
> past-day `/exec` refold + supervised task), the persist-confirmed hook
> sites in
> `crates/app/src/spot_1m_rest_boot.rs` (Dhan fire + sweep) and
> `crates/app/src/groww_spot_1m_boot.rs` (Groww fire + sweep), the
> config-gated spawn in `crates/app/src/main.rs` (shared-infra prefix),
> `crates/common/src/config.rs::RestCandleFoldConfig`
> (`[rest_candle_fold]`, serde default OFF; base.toml opts in with
> `catchup_days = 35`),
> `crates/common/src/error_code.rs::ErrorCode::RestCandleFold01Degraded`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `RestCandleFold01*` variant verbatim
> — `RestCandleFold01Degraded` and `FOLD-01` appear below.

---

## §0. Why this code exists (the producer-dead candles_* gap)

With BOTH live feeds retired (Dhan 2026-07-13, Groww 2026-07-15) the 21-TF
tick aggregator became publisher-less: the seal-writer + aggregator still
boot, but no tick ever reaches them, so `candles_1m..candles_1d` stopped
populating — while the operator's trading decisions depend on spot candles
across all timeframes being queryable in our own QuestDB, one month deep,
before pre-market (the §0 verbatim demands above).

The fold writer closes that: every **persist-CONFIRMED** `spot_1m_rest`
1m bar (handed off ONLY after the ILP flush ACK — a bar that never
persisted must not derive candles) is folded into all 21 timeframe buckets
on the `TfIndex::bucket_start` grid (o = first, h = max, l = min,
c = last, volume = checked i64 sum — exact-match parity with the
tf_consistency recompute is golden-tested), and every sealed bucket is
emitted as a `BufferedSeal` into the EXISTING shared seal-writer channel —
landing in the SAME `candles_*` tables with the SAME DEDUP key
(`ts, security_id, segment, feed`), so every emission is idempotent.
`tick_count` is 0 and `oi` is 0 HONESTLY (REST bars carry neither); the
pct-from-prev-day columns read 0.0 (the pct-stamping chain belonged to the
live path). This is NOT tick synthesis — no row ever touches `ticks`
(`live-feed-purity.md` rules 1-6 stand; rule 10's dated 2026-07-16 edit
permits THIS writer to produce `candles_1d`, sealed at the 15:30 close).

At boot, a catch-up re-folds the last `catchup_days` (default 35 — the
operator's one-month window + weekend slack; today + `catchup_days − 1`
past days, EXACTLY `catchup_days` days) of stored `spot_1m_rest` rows per
feed through the same engines, so the month of history the operator
demanded is derived into all 21 TFs even after a fresh clone — and TODAY's
rows additionally SEED the live day-map (below), so a mid-session restart
keeps a complete in-RAM refold source.

**Out-of-order/repair bars (2026-07-16 round-2 HIGH redesign; round-3
burst coalescing):** every bar received for the CURRENT trading day also
lands in a per-(feed, SID, segment) in-RAM **day-map** (minute →
last-received bar, last-write-wins; ≤375 entries × 8 keys — trivial
memory). A current-day repair — an out-of-order minute OR a value-UPDATE
of an already-folded minute — updates the map; the consumer loop drains
each arriving burst as ONE batch and refolds every dirty slot ONCE per
batch from its map through a fresh engine (microseconds, cold path — a
mid-day-outage sweep of N repairs costs one refold per slot, never N
full-day refolds), swaps the live engine, and re-emits every closed
bucket (DEDUP UPSERT heals in place). Lossless for bars received
IN-PROCESS THIS INCARNATION: the map holds every such bar, so the today
REPAIR path performs NO QuestDB read and adds no ILP-ACK → `/exec`
WAL-apply visibility race of its own (the round-1 `/exec` today-refold —
whose presence-only gate missed value-updates and whose read could miss
bars folded between mark and drain — is RETIRED). Honest crash-restart
residual (round-3 doc-honesty): the boot catch-up's TODAY seed IS an
`/exec` read — a minute persisted seconds before a crash-restart can be
WAL-invisible to that seed and stays out of the derived candles until
the NEXT boot's catch-up; the 15:40 IST tf-verify (Blind/mismatch) is
the pager for that window. A bar dated AFTER the wall-clock IST today
NEVER rolls the live day forward (the BOUNDARY-01 future-skew class —
dropped + counted `reason="future_dated"`, ONE coalesced coded error per
drained batch); with no current day yet (failed/empty catch-up) only a
TODAY-dated first bar is adopted — a past-dated first bar routes to the
past-day queue (a 1-bar adopted refold would force-seal + DEDUP-clobber
a previously-correct closed day). Only a repair for a day STRICTLY OLDER
than the engine's current day (day identity is the ENGINE's day, never
rolled past the wall clock — which kills the post-midnight clobber
class) takes the bounded, debounced `/exec` past-day refold: it emits
corrected CLOSED-day seals only, never touches a live engine, and keeps
the trigger-minute presence gate + bounded requeue (the only residual
SAME-SESSION WAL-lag surface).

**FOLD-01** is the typed record of any leg of that machinery degrading —
never of a healthy fold (which is counters + info lines only).

## §1. FOLD-01 — REST-era candle fold degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; the dirty-day refold and the next boot's catch-up are
DEDUP-idempotent repairs — the operator inspects, never manually
re-derives first).

**Trigger:** one of the fold legs failed
(`ErrorCode::RestCandleFold01Degraded`, distinguished by the `stage`
field):

| stage | Meaning |
|---|---|
| `catchup_query` | a boot catch-up / PAST-day refold `/exec` read failed — HTTP client build failure, transport error, non-2xx, or the STREAMED 8 MiB response cap refused the body (the cap is enforced chunk-by-chunk during the read — a chunked-transfer response without Content-Length can never buffer unbounded). That day/feed is skipped (counted); the next dirty mark or the next boot retries. Round-2 LOW-4: this stage (with `reason="no_http_client"`) also covers the drain finding NO HTTP client for the task incarnation — the queued past-day marks are dropped LOUDLY (counted under `dropped{reason="no_http_client"}`; the client is built once at task start and can never appear later, so keeping the marks would spin the debounce forever) and the next boot's catch-up re-derives. |
| `catchup_parse` | the `/exec` body was unparsable, the explicit row LIMIT was hit (a truncated day is NEVER partially folded — the tf_consistency tripwire discipline), or a poisoned segment value failed the allowlist (skipped, never re-queried). |
| `discovery_truncated` | the boot catch-up's per-feed instrument discovery hit ITS row LIMIT — a partial instrument set is never trusted; the whole (feed, day) fold pass is skipped LOUDLY (2026-07-16 hostile-review M4). |
| `refold_stale_read` | a PAST-day refold's `/exec` read did NOT yet contain the triggering repair minute (ILP-flush-ACK → WAL-apply visibility lag) — the refold is NOT emitted (would regress correct candles). Round-2 LOW-7 wording: `tv_rest_candle_fold_errors_total{stage="refold_stale_read"}` increments on EVERY failed attempt (each re-queue logs a coalesced `warn!`); the `error!` fires only on the EXHAUSTED arm (5 attempts). Round-2 scope: this stage exists ONLY on the past-day cold path — the current day refolds from the in-RAM day-map with no `/exec` read (2026-07-16 hostile-review M2 + round-2 HIGH). |
| `seal_send` | a sealed bucket could not be handed to the seal-writer channel (channel full past the boot-path pacing budget / global sender missing), or a confirmed-bar handoff was dropped (fold channel full/closed). Round-3: a CLOSED seal channel (seal-writer gone — shutdown/teardown) is labeled DISTINCTLY (`dropped{reason="seal_channel_closed"}`) on both the live and paced paths, never conflated with backpressure. |
| `future_dated` | (round-3 — the BOUNDARY-01 future-skew class) a live bar's IST date is AFTER the wall-clock IST today — it can never roll the live day forward; dropped + counted (`dropped{reason="future_dated"}`), ONE coalesced coded error per drained batch (≤1/minute at the legs' fire cadence). |
| `bad_identity` | (warn-level, once per process — coalesced) a spot-leg handoff carried an identity the fold's SecurityId space cannot represent (negative i64 — defensive); every occurrence is counted under `dropped{reason="bad_identity"}` (round-2 LOW-6 — previously a silent skip at the Groww hook sites). |
| `volume_saturated` | a TF bucket's volume i64 add saturated at `i64::MAX` — the fold stays ATOMIC (never a torn bucket), the saturated value is emitted honestly, one coalesced warn per (feed, SID, day) (2026-07-16 hostile-review M3). |
| `receiver_lost` | the fold task started but the shared receiver slot was EMPTY — a previous incarnation failed to re-park it (the RAII guard makes this near-unreachable); the task exits LOUDLY, never a silent clean_exit (2026-07-16 hostile-review HIGH-2). |
| `sender_install` | main.rs could not install the global fold-bar sender (already installed — a double-spawn class bug); the hook sites would feed a stale channel (LOW, 2026-07-16). |
| `task_respawn` | the supervised fold task died and was respawned (house pattern; `tv_rest_candle_fold_task_respawn_total{reason}`). Unwind builds only — release panics abort the process. |

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `FOLD-01`; the payload names
   the `stage`, the feed/SID/date where applicable, and the drop counts.
2. `catchup_query` / `catchup_parse` sustained → QuestDB `/exec` is
   degraded; run `make doctor` (cross-check BOOT-01/BOOT-02,
   WAL-SUSPEND-01 — the sibling post-market readers share the target).
   A LIMIT-hit (`catchup_parse`) on a legitimate day means the day's
   `spot_1m_rest` row count outgrew the 500-row envelope — raise the
   named constant in a reviewed PR, never silently.
3. `seal_send` with `reason="seal_channel_full"` → the seal-writer is
   backed up; cross-check AGGREGATOR-SEAL-01 / AGGREGATOR-DROP-01 and
   QuestDB ILP health. With `reason="no_seal_sender"` → a boot-ordering
   regression (the seal-writer must spawn before the fold task) — the
   wiring guard pins this order.
4. `task_respawn` flapping → a real bug in the fold loop; capture the
   backtrace in `data/logs/errors.jsonl.*` preceding the FOLD-01 line.
5. Verify recovery: `mcp__tickvault-logs__questdb_sql "select count(*)
   from candles_5m where ts > dateadd('d', -1, now())"` — rows land again
   once the legs re-fire (a healthy session ≈ 75 5m rows per SID per
   feed).

**Counters (static/bounded labels only):**
- `tv_rest_candle_fold_seals_total{feed}` — sealed buckets delivered to
  the seal-writer channel.
- `tv_rest_candle_fold_catchup_rows_total{feed}` — 1m rows folded by the
  boot catch-up / refold legs.
- `tv_rest_candle_fold_errors_total{stage}` — degrade legs (the taxonomy
  above).
- `tv_rest_candle_fold_dropped_total{reason}` —
  `channel_full` / `channel_closed` (confirmed-bar handoff),
  `seal_channel_full` / `seal_channel_closed` / `no_seal_sender` (seal
  emission — round-2 LOW-1: a CLOSED seal channel counts EVERY remaining
  seal of the slice, never just one; round-3: closed is its OWN reason on
  both the live and paced paths — shutdown, not backpressure),
  `out_of_session` (bars outside [09:15, 15:30) IST — skipped honestly),
  `future_dated` (round-3: a bar dated after the wall-clock IST today —
  never rolls the live day forward; the BOUNDARY-01 future-skew class),
  `bad_identity` (identities that do not fit the fold's SID space —
  catch-up discovery rows AND the spot-leg handoff sites; counted +
  coalesced warn, never silent),
  `no_http_client` (round-2 LOW-4: past-day marks dropped loudly because
  the incarnation has no HTTP client).
- `tv_rest_candle_fold_refold_queued_total` — PAST-day marks queued for
  the debounced `/exec` refold (the current day never queues — round-2).
- `tv_rest_candle_fold_day_refolds_total{feed}` — current-day day-map
  refolds, one per DIRTY SLOT per drained repair batch (round-3 burst
  coalescing: N repairs in one batch cost one refold per slot, never N;
  each refold re-emits the day's closed buckets, DEDUP-idempotent —
  round-2 HIGH).
- `tv_rest_candle_fold_duplicate_bars_total` — value-identical
  redeliveries (`PartialEq` compare — a NaN field compares unequal and
  falls through to a harmless idempotent refold) of a bar already in the
  day-map (no-op; nothing new to derive).
- `tv_rest_candle_fold_paced_waits_total` — boot-path seal emissions that
  slept on a full seal channel (the 2026-07-16 HIGH-1 pacing: newest days
  first, sleep-and-retry the SAME seal up to a bounded per-(feed, SID,
  day) budget BEFORE counting any drop — a drop is real backpressure
  exhaustion, not a burst artifact; round-2 LOW-7: the 60s budget is per
  `refold_day` call, i.e. per (feed, SID, day)).
- `tv_rest_candle_fold_volume_saturated_total` — saturating volume adds
  (M3 — the fold never tears).
- `tv_rest_candle_fold_heartbeat_total` — the DENSE positive progress
  signal (HIGH-3): incremented every heartbeat interval by the live fold
  loop REGARDLESS of bar traffic, so "the fold task is alive" is a
  monotonically-advancing series alarm-ready for a future CloudWatch
  floor (delivery today: metrics-local, no alarm — see the boundary
  below).
- `tv_rest_candle_fold_task_respawn_total{reason}` — supervisor respawns.

**Honest envelope:** every degrade is RE-DERIVABLE — the `spot_1m_rest`
source rows stand, and the refold/catch-up re-emit idempotently; no market
data is ever lost by a fold failure. The fold derives candles ONLY from
what the REST legs captured: a minute the vendor never served (the
SPOT1M-01 `empty` class) has no 1m bar and therefore leaves its higher-TF
buckets folding over the bars that DO exist — same-shape-as-source, never
fabricated. `tick_count`/`oi`/pct columns are honest zeros (documented in
§0); consumers comparing REST-era vs live-era candles must expect that
split. Current-day repairs refold from the in-RAM day-map — lossless for
bars received in-process THIS INCARNATION, no QuestDB read on the repair
path (round-2 HIGH; round-3: one refold per dirty slot per drained
batch). Honest crash-restart residual (round-3): the boot catch-up's
TODAY seed IS an `/exec` read — a minute persisted seconds before a
crash-restart can be WAL-invisible to that seed and stays out of the
derived candles until the NEXT boot's catch-up; the 15:40 IST tf-verify
(Blind/mismatch) is the pager for that window. The remaining
SAME-SESSION WAL-lag surface is the PAST-day `/exec` refold (a repair
arriving after the day-map rolled — e.g. a post-midnight sweep), where the
ILP-flush-ACK → `/exec`-visibility lag (WAL apply) is gated (M2,
2026-07-16): the refold verifies the TRIGGERING repair minute is present
in the read before emitting — a stale read is NEVER emitted (would
regress correct candles); the mark re-queues with backoff, bounded at
5 attempts, then degrades loudly (`refold_stale_read`) and the next
boot's catch-up re-derives the day. Past-day refolds emit corrected
CLOSED-day seals only and never touch a live engine, so a stale past-day
read can never corrupt open buckets (the round-2 midnight-clobber class
is structurally dead). The boot catch-up is flagged O(days × SIDs × rows) COLD work
(~35 days × 4 SIDs × ≤375 rows per feed — bounded, one-shot at boot),
iterated NEWEST day first (HIGH-1 — under seal-channel backpressure the
most recent, decision-relevant days are derived first) with paced
emission (sleep-and-retry the SAME seal on a full channel, bounded per-day
budget) so a catch-up burst larger than the seal channel never silently
drops the tail; a drop past the budget is counted + logged honestly as
LOST FOR THIS SESSION (re-derived only by the NEXT boot's catch-up — the
old log's "boot catch-up re-derives them" same-session claim was
circular and is retired). The per-bar live fold is O(21) constant work
per minute. Release-build panics abort the process (`panic = "abort"`) —
the respawn arms self-heal in unwind (dev/test) builds only (the
TICK-FLUSH-01 precedent). Heartbeat liveness bound (2026-07-16 fix round,
honest): the heartbeat interval is created AFTER the boot catch-up
completes, so a long paced catch-up (many days × SIDs sleeping on a full
seal channel) emits ZERO `tv_rest_candle_fold_heartbeat_total` increments
for its whole duration — a liveness consumer must NOT treat
catch-up-window silence as task death; the catch-up's own coalesced
per-feed summary `info!` is the progress signal for that window.

**Delivery boundary (honest — no false-OK):** FOLD-01 is
**log-sink-only**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list (the paging drift guard sees
no drift). The operator's end-to-end signal for "are the candles there?"
is the EXISTING 15:40 IST tf-consistency verifier — which since 2026-07-16
(HIGH-3) classifies a zero-candles day **Blind (High Telegram)** whenever
`spot_1m_rest` carries rows for that (feed, day), so a silently-dead fold
can never read as an Info NoData day (`tf-consistency-error-codes.md` §2
dated note) — plus the counters above (the dense
`tv_rest_candle_fold_heartbeat_total` series is the alarm-ready liveness
floor for a future CloudWatch filter). Adding a CloudWatch log-filter
alarm is a flagged follow-up (one map entry + doc paragraph + cost note —
the SCOREBOARD-01 / FEED-REJECT-01 precedent).

**Retention first-sweep burst (dated honest note, 2026-07-16 round-2
MEDIUM):** the same PR moves the chain tables (`option_chain_1m` +
`option_contract_1m_rest`) from the 90-day Standard window into the 35-day
market-data class, which makes every chain partition aged 36..90 days
drop-eligible AT ONCE — up to ~110 DAY partitions (~4 GB) in the FIRST
post-merge sweep. No code change was needed: the sweep is bounded by
`max_partitions_per_run` (default 200 — the burst fits one run, never
exceeds the cap), processes partitions strictly serially (one gzip export
on temp disk at a time; ~minutes of wall clock, off-hours), and stays
FAIL-CLOSED — a degraded S3 leg means no verified copy ⇒ no drop, so the
worst case is "nothing freed yet", never data loss. Subsequent sweeps
return to the ~1-partition/day steady state. (File-local twin note:
`crates/storage/src/partition_archive.rs::CHAIN_MARKET_DATA_TABLES`.)

## §2. What a PR that violates this contract looks like (REJECT)

- Routes fold output into `ticks` or synthesizes ticks from bars
  (live-feed-purity rules 1-6 — the hard ban stands).
- Hands bars to the fold BEFORE the `spot_1m_rest` flush ACK (an
  unpersisted bar must never derive candles the audit record does not
  back).
- Folds an out-of-order bar DIRECTLY into the live engine instead of the
  day-map refold (would corrupt open buckets high/low against the
  ordered-fold contract).
- Re-introduces a `/exec`-based CURRENT-day refold that replaces the live
  engine (the round-2 HIGH: the WAL-apply read can miss value-updates and
  race-window bars — the in-RAM day-map is the only legal today source),
  or routes a past-day `/exec` refold's output into a live engine.
- Trusts a LIMIT-truncated day (partial fold) instead of degrading loudly.
- Fabricates `tick_count` / `oi` / pct values on REST-derived seals.
- Adds per-TF or per-SID metric labels (unbounded label cardinality — the
  static-label law).

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/app/src/rest_candle_fold.rs`
- `crates/common/src/error_code.rs` (any `RestCandleFold01*` variant)
- `crates/common/src/config.rs` (`RestCandleFoldConfig`) or
  `config/base.toml` `[rest_candle_fold]`
- The fold hook sites in `crates/app/src/spot_1m_rest_boot.rs` /
  `crates/app/src/groww_spot_1m_boot.rs`
- Any file containing `FOLD-01`, `RestCandleFold01Degraded`,
  `rest_candle_fold`, `ConfirmedBar`, `send_confirmed_bars`,
  `set_global_fold_bar_sender`, or `tv_rest_candle_fold_`
