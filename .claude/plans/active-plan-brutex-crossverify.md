# Implementation Plan: BruteX↔TickVault daily cross-verification (S3-CSV read-only, feed='groww', 15:50 IST)

**Status:** APPROVED
**Date:** 2026-07-12
**Approved by:** Parthiban (operator directive 2026-07-12, verbatim quotes recorded in `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §37)

> **Guarantee matrices:** this plan carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (the canonical copy) — all 15 + 7 rows apply to every item in this plan,
> instantiated below in Design / Test Plan / Observability.
> Honest 100% claim (charter §F wording): 100% inside the tested envelope,
> with ratcheted regression coverage — a pure ratchet-tested comparer
> (paise-integer compare, hostile CSV parse, mapping normalization,
> keep-better rerun guard). NOT claimed: either side as ground truth
> (TickVault carries the known tick-conservation residual; Groww historical
> carries vendor-fill fluctuation — the report separates expected
> fluctuation from real divergence, never assuming live is correct); the
> live S3 + QuestDB legs are verified on the box at the first enabled run
> (no AWS creds in CI).

## Design

Post-close (15:50 IST, config-driven trigger with sanitize + auto-stop warn per
the `crates/app/src/feed_scoreboard_boot.rs` template) daily comparer that reads
BruteX-produced backtest 1m CSVs from
`s3://tv-prod-cold/crossverify/groww/<YYYY-MM-DD>/<segment>/<symbol>.csv` and
compares them against TickVault's own `candles_1m WHERE feed='groww'` — the §37
grant in `groww-second-feed-scope-2026-06-19.md` (operator 2026-07-12).

1. **S3 read (cold path only):** new pinned workspace dep `aws-sdk-s3` (exact
   version resolving to the aws-smithy set already locked by
   `aws-config`/`aws-sdk-ssm`/`aws-sdk-sns`; `default-features = false` +
   `["behavior-version-latest", "default-https-client", "rt-tokio"]` — the
   sibling feature set), client construction copying
   `crates/core/src/auth/secret_manager.rs::create_ssm_client` (default
   credential chain — instance role on prod, zero IAM change: the role
   already carries GetObject/ListBucket on `tv-prod-cold`). ListObjectsV2
   (paginated, ≤2000-object cap per date prefix) + GetObject with a 5 MB
   body cap + row cap per file; over-cap = degraded, never OOM.
2. **Config:** new `[brutex_crossverify]` section in `config/base.toml` +
   `BrutexCrossverifyConfig` in `crates/common/src/config.rs` —
   `#[serde(default)] enabled: bool` (default **false**; missing section =
   byte-identical behaviour), `tolerance_paise = 0`, `compare_volume = false`
   (hard NO-OP for feed='groww' — live Groww candle volume is always 0; a
   `true` flip is refused loudly), `bucket = "tv-prod-cold"`,
   `prefix = "crossverify/groww"`, `trigger_secs_of_day_ist = 57_000`
   (15:50:00), figment round-trip + defaults + B12 rollback tests.
3. **Mapping (pure, unit-tested):** join BruteX `symbol` → Groww `security_id`
   via `instrument_lifecycle WHERE feed='groww' AND dry_run=false` — stocks by
   `symbol_name` (`instrument_type='EQUITY'`), indices by prefix-stripped +
   `canonicalize_index_symbol`-normalized name (`instrument_type='INDEX'`),
   futures by `(underlying_symbol, expiry_date)` contract identity
   (`instrument_type='FUTIDX'`); the S3 `<segment>` path component is
   ADVISORY only, never the join key; symbols never matched raw across
   classes; unmapped/ambiguous symbols counted + named (bounded sample).
4. **Compare (pure):** per (symbol, minute) integer-paise OHLC-only equality
   (`(v * 100.0).round() as i64`, tolerance_paise applied on the integer
   diff; non-finite values rejected at parse). Categories: matched /
   diverged (per field) / missing_backtest / missing_live / out_of_session
   (backtest bars outside [09:15, 15:30) IST, window-filtered) /
   tail_unsealed (15:28 + 15:29 compared only when the live bar exists —
   the Groww close-time force-seal gap; the Dhan-only 15:30:05 seal is
   verified in main.rs Task 3b). Volume is stored both sides in audit rows
   but never classified (Quote 3). QuestDB reads use the micros-literal
   window lock (nanos÷1000 in WHERE, ×1000 rescale on projection) for BOTH
   `ts` and `expiry_date` predicates.
5. **Persistence (crates/storage):** two new audit tables via ILP-over-HTTP
   (`http::addr=`), ts-first DEDUP:
   - `brutex_crossverify_cell_audit` — DEDUP
     `ts, trading_date_ist, feed, security_id, segment, minute_ts_ist, kind, field`
     (kind AND field in-key so O/H/L/C cells + missing/divergence rows never
     collapse); non-key `observed_at` wall-clock column so re-runs are
     traceable; stale-resolved-cell linger documented (page/runbook trust
     only cells whose count matches the daily row).
   - `brutex_crossverify_daily_audit` — one row per (trading_date, feed) at a
     deterministic 15:50:00 IST row ts (re-runs UPSERT in place), with a
     keep-better outcome guard (a degraded/NO_DATA rerun never downgrades a
     measured terminal row; `stage="outcome_regression"` logged on
     suppression).
6. **Error codes (crates/common):** `BRUTEX-XVERIFY-01` (divergence found /
   report leg degraded, High, severity-independent `is_auto_triage_safe`
   override to false) + `BRUTEX-XVERIFY-02` (S3/QuestDB fetch degraded,
   High) — new prefix added to `has_known_prefix` with a dated comment,
   catalogue count bumped, companion rule file
   `.claude/rules/project/brutex-crossverify-error-codes.md` in the SAME
   code PR (crossref test lockstep). Log-sink-only (no CloudWatch alarm
   entry); the Telegram summary is the operator signal.
7. **Telegram (crates/core):** `BrutexCrossverifySummary` (Info, Immediate,
   10-commandments body — BLIND wording on compared==0, expected-fluctuation
   vs real-divergence split quantified) + `BrutexCrossverifyAborted { detail }`
   (High) sibling via the inner/outer supervisor idiom.
8. **Web surface (crates/api):** public read-only GET `/crossverify` HTML page
   on the existing api server (Quote 2 + the 2026-06-23 public-read
   precedent) — server-rendered from the daily summary artifact,
   `html_escape` on ALL CSV-derived text (BruteX symbols are EXTERNAL
   input), strict fail-closed `?date=` validation
   (`is_valid_trading_date` precedent), row-capped output.
9. **Runner (crates/app):** `brutex_crossverify_boot.rs` — supervised
   once-per-day task in the process-global prefix, spawned on BOTH boot
   paths (slow prefix + fast crash-recovery arm), pure decide fn with
   RunCatchUp on late boot, `TICKVAULT_BRUTEX_XVERIFY_NOW` /
   `TICKVAULT_BRUTEX_XVERIFY_DATE` force/backfill env vars (fail-closed
   validation), deterministic run ts, bounded S3 re-poll for a late BruteX
   publish (deadline 15:45 IST; absent at 15:50 → re-poll to 16:05 → loud
   NO_DATA), total run wall-clock capped at 16:05 IST (inside the 16:15
   warn / 16:30 instance auto-stop bounds).

Crates touched by the follow-up code PR: `crates/common` (config +
ErrorCode), `crates/core` (mapping + Telegram events), `crates/storage`
(audit tables), `crates/api` (`/crossverify` page), `crates/app` (runner +
S3 reader + wiring). THIS PR is docs-only (rule amendment + this plan).

## Edge Cases

- **Empty / partial S3 prefix:** no objects at 15:50 → bounded re-poll to
  16:05, then NO_DATA (loud, never PASS — Rule 11: compared==0 is always
  BLIND). Partial upload race → require/prefer the `_MANIFEST.json`
  publish-complete marker; a missing manifest with files present is
  classified PARTIAL, honestly stamped.
- **Hostile CSV content:** BOM strip, CRLF/LF/CR normalization, quoted
  commas, non-UTF-8 rejection, header-row validation, NaN/inf rejected at
  parse (never into a compare or an audit row), duplicate minutes in one
  file (first-row-wins + counted), body-bytes cap (5 MB) + rows-per-file
  cap; hostile symbol text (XSS vectors, BiDi/control chars) passes
  `sanitize_audit_string` before ILP write and `html_escape` before any
  HTML/Telegram body.
- **Mapping holes:** unmapped symbols (BSE_EQ stocks don't exist in the
  Groww watch set — counted, never fatal), ambiguous joins (>1 lifecycle
  row → fail-closed skip + counted), index alias drift (`NIFTYAUTO` vs
  "NIFTY AUTO" — normalize both sides: strip spaces/hyphens, uppercase;
  name unmapped indices in the summary), futures symbol form variants
  (`NIFTY-2026-07-30-FUT`, `NSE-`/`BSE-` prefixed forms tolerated),
  index-vs-future prefix collision prevented by the `instrument_type`
  filter per class.
- **Time semantics:** DST-free IST throughout (fixed +19800s); minute-OPEN
  label convention pinned in the §37.3 producer contract; out_of_session
  backtest bars filtered; tail_unsealed for 15:28/15:29 when the live bar
  is absent; micros-literal SQL trap covered for every TIMESTAMP predicate.
- **Scheduling:** non-trading day → skip (env DATE backfill refuses
  non-trading/future targets); late boot (past 15:50 on a trading day) →
  RunCatchUp once; rerun-after-fix → deterministic ts UPSERT + keep-better
  guard (a rerun that measured nothing never erases a measured day).

## Failure Modes

- **S3 unreachable / auth failure:** degraded run — `error!` with
  `code = BRUTEX-XVERIFY-02` + stage label, daily row `NO_DATA`/`PARTIAL`
  (sentinels, never fabricated zeros), retry is next day (or NOW/DATE env
  backfill). Never blocks boot, never touches the hot path.
- **QuestDB down at 15:50:** `BRUTEX-XVERIFY-02` staged (`stage="questdb"`),
  run degrades; ensure-DDL failures degrade-never-halt (idempotent next
  boot).
- **Persist/flush failure:** `error!` (never `warn!` — error-level
  meta-guard) with code + stage, counter incremented; skip-when-empty final
  flush (the 2026-06-10 false-alarm lesson).
- **Task panic:** the outer supervisor classifies the JoinError and pages
  `BrutexCrossverifyAborted { detail }` (High) — the daily deliverable can
  never silently vanish; cancellation at shutdown is info-only.
- **BruteX publishes late/never:** bounded re-poll window (15:50 → 16:05),
  then loud NO_DATA daily row + summary; never a pass, never a retry storm
  into the 16:30 instance auto-stop.
- **Outcome regression on rerun:** keep-better guard suppresses any
  downgrade of a measured terminal daily row, logging
  `stage="outcome_regression"` (the scoreboard incident-class fix).

## Test Plan

Per `.claude/rules/project/testing-scope.md`, scoped to the touched crates;
categories from the 22-test taxonomy that apply: unit, integration,
property (proptest on the CSV parser + paise compare), boundary (financial
guard — comparer fns matching `candle|ohlc|price` get boundary-named
tests), source-scan ratchets.

- **crates/common:** config defaults + figment round-trip + B12 rollback
  (`enabled = false` sticks; missing section = full defaults); ErrorCode
  catalogue tests (count bump, prefix chain, runbook-on-disk, crossref,
  auto-triage override).
- **crates/core:** mapping normalization unit tests (stock/index/future
  classes, alias drift, prefix collision, ambiguity fail-closed); Telegram
  body/topic/severity/dispatch tests incl. BLIND wording on compared==0.
- **crates/storage:** DDL needles, DEDUP-key token tests (ts-first, feed,
  segment-with-security_id, kind+field in key — dedup_segment_meta_guard
  compliant), ILP-HTTP conf pin, disconnected-flush-Errs, keep-better
  guard unit tests.
- **crates/api:** `/crossverify` handler literal/needle tests (escaping of
  hostile symbols, date validation fail-closed, row cap) — the api crate
  coverage floor (98.6) requires near-full handler coverage.
- **crates/app:** decide-fn boundary tests (trigger/exact/late/non-trading/
  force), CSV parse proptest (arbitrary mutations parse-or-reject-cleanly),
  paise-compare boundary tests (0.005 rounding, negative, extreme),
  re-poll/wall-clock-cap tests, `test_brutex_crossverify_is_wired_into_main`
  source-scan ratchet (both boot paths).
- **Live legs (S3 + QuestDB on the box):** verified at the first enabled
  run on prod — stated as not-yet-measured until then, never asserted.

## Rollback

`[brutex_crossverify] enabled = false` is the DEFAULT — the feature is
opt-in and a missing/false section restores byte-identical behaviour (B12
rollback test pins this). The feature is additive-only: no existing table,
route, task, or config key is modified; the two new audit tables and the
new `/crossverify` route are inert when disabled. Full rollback = flip the
flag (no code revert needed); hard rollback = revert the follow-up code PR
(this docs-only PR carries no runtime behaviour at all).

## Observability

Per the 7-layer telemetry contract (see `per-wave-guarantee-matrix.md`):

- **ErrorCodes:** `BRUTEX-XVERIFY-01` / `BRUTEX-XVERIFY-02` with the
  companion runbook rule file (same code PR); every `error!` carries
  `code = ErrorCode::X.code_str()` + a `stage` field (tag-guard).
  Honest delivery boundary: log-sink-only today (no `error_code_alerts`
  entry) — the Telegram daily summary is the operator signal.
- **Telegram:** `BrutexCrossverifySummary` (Info, Immediate — quantifies
  the typical fluctuation and flags beyond-tolerance divergence, per
  Quote 1) + `BrutexCrossverifyAborted` (High) so the daily deliverable is
  never silently dropped.
- **Audit tables:** `brutex_crossverify_cell_audit` (per-cell forensic,
  kind+field in DEDUP key, non-key `observed_at`) +
  `brutex_crossverify_daily_audit` (daily verdict, keep-better guarded) —
  queryable via `mcp__tickvault-logs__questdb_sql`.
- **Counters:** `tv_brutex_crossverify_runs_total{status}`,
  `tv_brutex_crossverify_cells_total{kind}`,
  `tv_brutex_crossverify_degraded_total{stage}` (static labels only).
- **Web page:** public read-only GET `/crossverify` (escaped,
  date-validated, row-capped) reading the daily summary artifact.
- **File artifacts:** daily CSV + summary.json under `data/crossverify/`
  with the status-line-first convention (a header-only file can never read
  as a perfect day).

## Plan Items

- [ ] Item 1 — `[brutex_crossverify]` config section + `BrutexCrossverifyConfig` (default OFF, tolerance_paise=0, compare_volume=false hard-no-op, bucket/prefix/trigger)
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_brutex_crossverify_config_defaults_off, test_brutex_crossverify_flag_rollback_round_trip, test_compare_volume_refused_for_groww
- [ ] Item 2 — `aws-sdk-s3` pinned workspace dep + S3 reader (list/get with body + object caps, typed errors)
  - Files: Cargo.toml, crates/app/Cargo.toml, crates/app/src/brutex_crossverify_boot.rs
  - Tests: test_s3_key_builder_date_and_prefix, test_csv_body_cap_rejects_oversize, test_list_page_cap_bounds_objects
- [ ] Item 3 — hostile CSV parser + integer-paise OHLC comparer (pure)
  - Files: crates/app/src/brutex_crossverify_boot.rs
  - Tests: test_parse_rejects_non_finite_and_bad_header, test_parse_strips_bom_and_normalizes_line_endings, test_paise_compare_boundary_rounding, proptest_csv_arbitrary_mutations_parse_or_reject_cleanly
- [ ] Item 4 — symbol↔security_id mapping via instrument_lifecycle (stock/index/future classes, normalization, ambiguity fail-closed)
  - Files: crates/core/src/feed/groww/brutex_crossverify_map.rs
  - Tests: test_map_stock_by_symbol_name_equity_only, test_map_index_prefix_strip_and_canonicalize, test_map_future_by_underlying_and_expiry, test_ambiguous_symbol_fails_closed
- [ ] Item 5 — two audit tables (cell + daily) with ts-first DEDUP incl. feed+segment+kind+field, non-key observed_at, ILP-over-HTTP, keep-better daily guard
  - Files: crates/storage/src/brutex_crossverify_audit_persistence.rs
  - Tests: test_cell_dedup_key_tokens_ts_first_feed_segment_kind_field, test_ilp_conf_targets_http_port, test_daily_keep_better_suppresses_outcome_regression, test_disconnected_flush_errs
- [ ] Item 6 — ErrorCode variants BRUTEX-XVERIFY-01/02 + prefix chain + companion rule file `.claude/rules/project/brutex-crossverify-error-codes.md`
  - Files: crates/common/src/error_code.rs, .claude/rules/project/brutex-crossverify-error-codes.md
  - Tests: test_all_list_length_matches_catalogue_size, test_code_str_follows_expected_prefix_pattern, test_brutex_xverify_01_never_auto_triage
- [ ] Item 7 — Telegram events (Summary Info/Immediate + Aborted High) with BLIND wording + fluctuation-vs-divergence split
  - Files: crates/core/src/notification/events.rs
  - Tests: test_brutex_crossverify_summary_body_blind_on_zero_compared, test_brutex_crossverify_summary_quantifies_fluctuation, test_brutex_crossverify_aborted_severity_high
- [ ] Item 8 — public read-only `/crossverify` HTML page (escaped, date-validated, row-capped)
  - Files: crates/api/src/handlers/crossverify.rs, crates/api/src/lib.rs
  - Tests: test_crossverify_page_escapes_hostile_symbols, test_crossverify_date_param_fail_closed, test_crossverify_rows_capped
- [ ] Item 9 — 15:50 IST supervised runner: decide fn + RunCatchUp + NOW/DATE env force-run + deterministic run ts + bounded re-poll + 16:05 wall-clock cap + both-boot-paths wiring + ratchet
  - Files: crates/app/src/brutex_crossverify_boot.rs, crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs
  - Tests: test_decide_brutex_crossverify_start_boundaries, test_date_override_refuses_future_and_non_trading, test_repoll_capped_at_1605_ist, test_brutex_crossverify_is_wired_into_main
