# Feed Gap-Episode Forensics — Error Codes (FEED-GAP-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` (Groww is the sole live feed) >
> `live-feed-purity.md` (annotation NEVER repair — `candles_*` untouched) >
> this file.
> **Operator directive (2026-07-14, relayed via the coordinator session):**
> *"irrespective of any situation the Groww feed must never break"* — and when
> it DOES pause, every gap must become a NAMED, DURABLE, Telegram-visible
> episode instead of scattered log lines.
> **Companion code:** `crates/storage/src/feed_gap_audit_persistence.rs`
> (the `feed_gap_audit` table DDL + ILP-over-HTTP writer),
> `crates/app/src/groww_bridge.rs` (the gap-episode tracker riding the
> existing liveness poll), `crates/app/src/feed_scoreboard_boot.rs` (the
> 15:45 IST dangling-close sweep),
> `crates/common/src/error_code.rs::ErrorCode::FeedGap01EpisodeDegraded`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `FeedGap01*` variant verbatim —
> `FEED-GAP-01` and `FeedGap01EpisodeDegraded` appear below.

---

## §0. Why this code exists (the 2026-07-14 34.999s incident)

On 2026-07-14 the Groww feed went silent for a measured **34.999 seconds**
mid-session. Detection EXISTED — the stall watchdog killed + relaunched the
sidecar, per-restart warn lines and counters fired — but the gap itself was
never a first-class object: no durable row said "the feed was dark from
HH:MM:SS to HH:MM:SS, N restarts fired, these 1-minute buckets are partial."
Reconstructing the episode meant grepping scattered logs across the stall
watchdog, the bridge, and the aggregator — the exact forensic-hole class the
operator's standing demand forbids. This subsystem turns every Groww feed gap
into a **named, durable, Telegram-visible episode**:

1. **OPEN row** written to the new `feed_gap_audit` QuestDB table at drop
   DETECTION — the feed-level last-tick age crosses
   `FEED_GAP_EPISODE_THRESHOLD_SECS` (= 10) during market hours.
2. **CLOSE row** written at the recovery edge (liveness advances again),
   carrying the measured `gap_secs`, the `kill_count` (stall-watchdog
   restarts inside the episode; `-1` sentinel when not cheaply measurable),
   and the NAMED partial 1-minute buckets the gap overlapped.
3. **ONE Telegram bubble per episode** — opened (High, pages once) +
   closed (Info) — never per-poll spam (audit Rule 4 edge discipline).
4. The unconditional counter `tv_feed_gap_seconds_total` accumulates ALL
   measured liveness gaps regardless of the 10s episode threshold, so
   sub-threshold micro-gaps stay visible as a trend without episode noise.

**FEED-GAP-01** is the typed record of the machinery itself degrading —
never of the gap (the gap is the episode rows + Telegram, not an error code).

## §1. FEED-GAP-01 — gap-episode forensics degraded

**Severity:** Medium. **Auto-triage safe:** Yes (the subsystem is
ANNOTATION ONLY — it is never on the feed's recovery path, never on the tick
hot path, and a failed write loses only the forensic row for that edge; the
stall watchdog, the reconnect ladder, and the capture chain are untouched).

**Trigger:** one of the gap-episode legs failed
(`ErrorCode::FeedGap01EpisodeDegraded`, distinguished by the `stage` field):

1. `stage="ensure_client_build"` / `stage="ensure_ddl"` — the boot-time
   `feed_gap_audit` ensure-DDL could not run (HTTP-CLIENT-01 class; the
   usual duplicate-row-window consequence until a later boot's ensure
   succeeds).
2. `stage="append"` / `stage="flush"` — an OPEN/CLOSE row could not be
   persisted (ILP-over-HTTP down, QuestDB unreachable, server reject on the
   per-request ACK). The episode still fires its Telegram bubble; only the
   durable row is missing until the next edge re-appends.
3. `stage="dangling_close"` — the 15:45 IST scoreboard sweep could not
   close a dangling OPEN episode.
4. Any other degraded machinery arm (tracker/channel) — named in the
   payload, never silent.

Counter: `tv_feed_gap_audit_write_errors_total`.

**The `feed_gap_audit` table (schema contract):**

| Column | Meaning |
|---|---|
| `ts` TIMESTAMP | designated timestamp (row emit instant) |
| `trading_date_ist` | the IST trading day |
| `feed` SYMBOL | `groww` (feed-agnostic by construction) |
| `start_ts` TIMESTAMP | gap detection instant (threshold crossing) |
| `end_ts` TIMESTAMP | recovery instant (null/absent on OPEN rows) |
| `gap_secs` LONG | measured gap; `-1` sentinel = unknown (OPEN / dangling) |
| `kill_count` INT | stall-watchdog restarts inside the episode; `-1` = unknown |
| `partial_minutes` STRING | comma list of 1m bucket labels overlapped by [start, end], bounded |
| `outcome` SYMBOL | `open` / `closed` / `dangling_closed` |
| **DEDUP UPSERT KEYS** | `(ts, trading_date_ist, feed, start_ts, outcome)` — `outcome` in-key per phase-0 template rule 3, so the OPEN row and the CLOSE row of one episode BOTH survive (a lifecycle-chain audit, never overwrite) |

**Dangling-close contract:** an episode whose CLOSE edge never fires (the
feed dies into the session tail, the box stops at 16:30 IST, or the process
is killed mid-gap — the dark-hole class) is closed by the 15:45 IST
scoreboard aggregation: every OPEN row of the day lacking a matching CLOSE
gets a `dangling_closed` row with `-1` sentinels for `gap_secs` /
`kill_count` (never fabricated measurements — Rule 11).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `FEED-GAP-01`; the `stage`
   names the failing leg.
2. `tv_feed_gap_audit_write_errors_total` rate non-zero → QuestDB ILP/HTTP
   degraded; run `make doctor` (cross-check BOOT-01/BOOT-02 if at boot).
3. The gap episodes themselves:
   `mcp__tickvault-logs__questdb_sql "select * from feed_gap_audit where
   trading_date_ist = today() order by ts"` — the day's full episode chain.
4. The feed's own recovery is owned by FEED-STALL-01 /
   FEED-SUPERVISOR-01 — this code never implies the feed is stuck.

**Honest envelope:** ANNOTATION, NEVER REPAIR — the `candles_*` tables are
untouched (live-feed purity); a partial 1m bucket is NAMED, never
back-filled or fabricated. The write is best-effort and never blocks or
delays the reconnect/recovery machinery. Gap measurement is the feed-level
last-tick age (whole-universe liveness, the ILLIQUID-vs-DEAD rule of
`feed-stall-watchdog-error-codes.md` §0) — a single quiet instrument is not
a gap. The episode check rides the bridge's existing periodic poll, not the
per-tick hot path.

**Delivery boundary (operator-approved default, 2026-07-14):**
FEED-GAP-01 is **Telegram-episode-only** — deliberately NO CloudWatch log
metric filter / alarm (cost + noise-aversion; the per-episode Telegram
bubble is the operator signal, and FEED-STALL-01's pagers already own the
"feed keeps dying" escalation). Adding a CloudWatch route later is a single
`error_code_alerts` map entry in `deploy/aws/terraform/error-code-alarms.tf`
plus a dated note here.

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `FeedGap01*` variant)
- `crates/storage/src/feed_gap_audit_persistence.rs`
- `crates/app/src/groww_bridge.rs` (the gap-episode tracker)
- `crates/app/src/feed_scoreboard_boot.rs` (the dangling-close sweep)
- Any file containing `FEED-GAP-01`, `FeedGap01`, `feed_gap_audit`,
  `FEED_GAP_EPISODE_THRESHOLD_SECS`, `tv_feed_gap_seconds_total`, or
  `tv_feed_gap_audit_write_errors_total`
