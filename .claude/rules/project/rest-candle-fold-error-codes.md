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
> + confirmed-bar handoff + boot catch-up + dirty-day refold + supervised
> task), the persist-confirmed hook sites in
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
operator's one-month window + weekend slack) of stored `spot_1m_rest` rows
per feed through the same engines, so the month of history the operator
demanded is derived into all 21 TFs even after a fresh clone. Out-of-order
bars (backfill/sweep repairs) are never folded into live RAM state — the
affected (feed, SID, segment, day) is marked dirty and a debounced refold
re-reads that day from QuestDB and re-emits (DEDUP UPSERT heals in place).

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
| `catchup_query` | a boot catch-up / dirty-day refold `/exec` read failed — HTTP client build failure, transport error, non-2xx, or the 8 MiB response cap refused the body. That day/feed is skipped (counted); the next dirty mark or the next boot retries. |
| `catchup_parse` | the `/exec` body was unparsable, the explicit row LIMIT was hit (a truncated day is NEVER partially folded — the tf_consistency tripwire discipline), a poisoned segment value failed the allowlist (skipped, never re-queried), or a volume checked-add overflowed during a refold. |
| `seal_send` | a sealed bucket could not be handed to the seal-writer channel (channel full / global sender missing), a confirmed-bar handoff was dropped (fold channel full/closed), or a LIVE volume checked-add overflowed (bucket dropped — the refold re-derives it). |
| `task_respawn` | the supervised fold task died and was respawned (house pattern; `tv_rest_candle_fold_task_respawn_total{reason}`). |

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
  `seal_channel_full` / `no_seal_sender` (seal emission),
  `out_of_session` (bars outside [09:15, 15:30) IST — skipped honestly).
- `tv_rest_candle_fold_refold_queued_total` — dirty-day marks queued for
  the debounced refold.
- `tv_rest_candle_fold_task_respawn_total{reason}` — supervisor respawns.

**Honest envelope:** every degrade is RE-DERIVABLE — the `spot_1m_rest`
source rows stand, and the refold/catch-up re-emit idempotently; no market
data is ever lost by a fold failure. The fold derives candles ONLY from
what the REST legs captured: a minute the vendor never served (the
SPOT1M-01 `empty` class) has no 1m bar and therefore leaves its higher-TF
buckets folding over the bars that DO exist — same-shape-as-source, never
fabricated. `tick_count`/`oi`/pct columns are honest zeros (documented in
§0); consumers comparing REST-era vs live-era candles must expect that
split. The dirty-day refold reads QuestDB, so the ILP-flush-ACK →
`/exec`-visibility lag (WAL apply) can make an IMMEDIATE refold miss the
newest row — the next dirty mark or the boot catch-up covers it (a
documented residual, not silent loss). The boot catch-up is flagged
O(days × SIDs × rows) COLD work (~35 days × 4 SIDs × ≤375 rows per feed —
bounded, one-shot at boot); the per-bar live fold is O(21) constant work
per minute. Release-build panics abort the process (`panic = "abort"`) —
the respawn arms self-heal in unwind (dev/test) builds only (the
TICK-FLUSH-01 precedent).

**Delivery boundary (honest — no false-OK):** FOLD-01 is
**log-sink-only**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list (the paging drift guard sees
no drift). The operator's end-to-end signal for "are the candles there?"
is the EXISTING 15:40 IST tf-consistency verifier (TF-VERIFY-01/02 — the
recompute-vs-stored comparison covers fold output exactly like live
output) plus the counters above. Adding a CloudWatch log-filter alarm is a
flagged follow-up (one map entry + doc paragraph + cost note — the
SCOREBOARD-01 / FEED-REJECT-01 precedent).

## §2. What a PR that violates this contract looks like (REJECT)

- Routes fold output into `ticks` or synthesizes ticks from bars
  (live-feed-purity rules 1-6 — the hard ban stands).
- Hands bars to the fold BEFORE the `spot_1m_rest` flush ACK (an
  unpersisted bar must never derive candles the audit record does not
  back).
- Folds an out-of-order bar into live RAM state instead of the dirty-day
  refold (would corrupt open buckets high/low against the ordered-fold
  contract).
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
