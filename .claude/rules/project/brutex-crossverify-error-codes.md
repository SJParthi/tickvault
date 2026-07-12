# BruteX↔TickVault Daily Cross-Verification — Error Codes (BRUTEX-XVERIFY-01 / BRUTEX-XVERIFY-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §33 (Groww LIVE-FEED-ONLY — this
> feature makes NO Groww API call of any kind; the comparison source is
> BruteX-produced CSVs on S3, not a Groww historical pull) > this file.
> **Companion code:** `crates/common/src/config.rs::BrutexCrossverifyConfig`
> (`[brutex_crossverify]`, default OFF),
> `crates/app/src/brutex_crossverify_boot.rs` (the 15:50 IST daily task) +
> `crates/app/src/brutex_crossverify_compare.rs` (pure compare core),
> `crates/storage/src/brutex_crossverify_persistence.rs` (forensic tables),
> `crates/common/src/error_code.rs::ErrorCode::{BrutexXverify01DivergenceFound,
> BrutexXverify02RunDegraded}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `BrutexXverify0*` variant verbatim —
> `BrutexXverify01DivergenceFound`, `BrutexXverify02RunDegraded`,
> `BRUTEX-XVERIFY-01` and `BRUTEX-XVERIFY-02` appear here.

---

## §0. Why these codes exist (the second-opinion parity signal)

BruteX (the sibling system, same Groww account) independently produces
1-minute OHLC CSVs from its own Groww capture and publishes them daily to
`s3://tv-prod-cold/crossverify/groww/<date>/...`. At **15:50 IST** each
trading day (config `trigger_secs_of_day_ist = 57_000`), TickVault fetches
those CSVs (read-only S3 GET — the instance role already holds
`s3:GetObject`/`ListBucket` on `tv-prod-cold`; ZERO IAM change) and
cross-verifies them, minute-by-minute, against the LIVE `candles_1m` rows
tagged `feed='groww'`. Prices compare as paise integers with an INCLUSIVE
tolerance (`|live − brutex| <= price_tolerance_paise`, default 0 = exact).
Results land in:

1. QuestDB `brutex_crossverify_cell_audit` — one forensic row per divergent
   cell, DEDUP `(ts, trading_date_ist, feed, symbol, security_id,
   exchange_segment, minute_ts_ist, kind, field)` (I-P1-11 + feed-in-key);
   `observed_at` is a NON-key wall-clock column.
2. QuestDB `brutex_crossverify_daily` — one per-day summary row with a
   keep-better guard (a measured `clean|diverged|partial` outcome is never
   overwritten by a later `no_data|blind|degraded` re-run —
   `stage="outcome_regression"` logs the suppression).
3. One Telegram summary (compared / diverged / missing / degraded counts).
4. The public read-only `GET /crossverify` page.

The whole subsystem is COLD-PATH, default-OFF, and best-effort — never on
the tick hot path, never on any order/recovery path.

## §1. BRUTEX-XVERIFY-01 — divergence found

**Severity:** High. **Auto-triage safe:** **No** — severity-independent
override in `is_auto_triage_safe()` (the FUTIDX-02 / WAL-SUSPEND-01
precedent): a data-comparability signal is never auto-actioned; the
operator decides which capture chain (TickVault live vs BruteX) is wrong.

**Trigger:** the daily run found ≥1 divergent cell —
`ErrorCode::BrutexXverify01DivergenceFound`. Cell kinds:

| kind | Meaning |
|---|---|
| `diverged` | both sides have the minute; a field differs beyond tolerance |
| `missing_live` | BruteX has the minute, live `candles_1m` (feed='groww') does not |
| `missing_brutex` | live has the minute, the BruteX CSV does not |
| `tail_unsealed` | 15:28/15:29 minute absent live (close-seal timing) — informational, never `missing_live` |
| `out_of_session` | a BruteX row outside [09:15, 15:30) IST — recorded, not classified |

**Why non-zero is not necessarily a bug:** both sides sample the SAME Groww
LTP stream at different capture points; per-minute high/low can legitimately
differ by delivery skew. Track the trend (the daily table is the trend
store) — a stable small baseline is skew; a spike or sustained open/close
drift is a real capture problem on one side.

**Volume honest note:** Groww live volume is always 0 (LTP-only feed), so
volume classification is HARD-REFUSED for the groww feed regardless of
`compare_volume`; both sides are still stored for forensics.

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select field, kind, count(*) from
   brutex_crossverify_cell_audit where trading_date_ist = today() group by
   field, kind"` — which field/kind dominates.
2. Open `GET /crossverify` for the per-day trend and the day's worst
   symbols.
3. Cross-check the day's Groww feed health (FEED-STALL-01, WS-GAP-06,
   the 15:45 scorecard) — a live-side gap usually correlates with a stall
   episode; a BruteX-side gap shows as `missing_brutex` clusters.
4. Do NOT hand-patch either table — both re-produce tomorrow; the audit
   rows are the durable record.

## §2. BRUTEX-XVERIFY-02 — run degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; the run is DEDUP-idempotent and the keep-better guard protects a
previously measured day — the operator inspects at leisure).

**Trigger:** a leg of the daily run failed —
`ErrorCode::BrutexXverify02RunDegraded`, `stage` on the payload:

| stage | Meaning |
|---|---|
| `s3_list` / `s3_fetch` | S3 ListObjectsV2/GetObject failed after the bounded 3-attempts-per-object ladder |
| `object_too_large` / `too_many_keys` | the 5 MiB per-object cap or the 2,000-key cap refused a runaway/hostile producer |
| `csv_parse` | a BruteX CSV row was malformed (NaN/inf/|price|>1e7 rupees rejected at parse) |
| `wall_clock_cap` | 16:05 IST (`deadline_secs_of_day_ist = 57_900`) passed while the prefix was still empty/incomplete — the day records `no_data` |
| `questdb_read` / `questdb_write` | the live-candle SELECT or the forensic ILP-over-HTTP write failed |
| `outcome_regression` | a rerun tried to downgrade a measured daily outcome — suppressed, never applied |

Empty prefix at 15:50 → re-poll every 120s (`repoll_interval_secs`) until
16:05, then `no_data` — never a silent skip, never an infinite wait.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `BRUTEX-XVERIFY-02`; the
   `stage` names the failed leg.
2. `s3_*` stages: check the BruteX producer published today's prefix
   (`aws s3 ls s3://tv-prod-cold/crossverify/groww/<date>/`) and the
   instance role's S3 reachability.
3. `questdb_*` stages: `make doctor` (cross-check BOOT-01/BOOT-02).
4. Backfill once healthy: the run is DEDUP-idempotent; the keep-better
   guard prevents a blind rerun from erasing a measured day.

**Honest envelope:** the cross-verify is evidence, not proof — it compares
two captures of the same sampled vendor stream; it cannot see ticks Groww
never delivered to either side. A degraded/no-data day is honestly stamped
(`partial`/`no_data`/`degraded`), never reported clean on an empty compare
set (audit Rule 11). Delivery boundary: both codes are log-sink-only today
(no `error_code_alerts` map entry); the Telegram daily summary is the
operator signal.

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `BrutexXverify0*` variant)
- `crates/common/src/config.rs` (`BrutexCrossverifyConfig`)
- `crates/app/src/brutex_crossverify_boot.rs` /
  `crates/app/src/brutex_crossverify_compare.rs`
- `crates/storage/src/brutex_crossverify_persistence.rs`
- Any file containing `BRUTEX-XVERIFY-`, `BrutexXverify0`,
  `brutex_crossverify_cell_audit`, `brutex_crossverify_daily`, or
  `BrutexCrossverifyConfig`
