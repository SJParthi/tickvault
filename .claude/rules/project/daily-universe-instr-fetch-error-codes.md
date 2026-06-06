# Daily Universe — INSTR-FETCH Error Codes (Sub-PR #9)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C > this file.
> **Companion code:** `crates/common/src/error_code.rs::ErrorCode::InstrFetch{01,02,03,04}*`.
> **Companion contract:** `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §3, §4, §18, §22, §26.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to mention every `InstrFetch*` variant.

---

## §0. Why these codes exist

The daily-universe fetch chain (Sub-PRs #3 + #4 + #5 + #6 + #7) drives boot from raw bytes → validated CSV → unique underlyings → indices → 250-SID universe. Every error path in that chain MUST fail-closed with a typed `ErrorCode` so the orchestrator (Sub-PR #10) can route the failure to:

1. Telegram (operator paged via Severity::Critical)
2. CloudWatch `tv_instr_fetch_errors_total{code, stage}` counter
3. `instrument_fetch_audit` forensic row (Sub-PR #9b — table schema follow-on)
4. The §4 infinite-retry / §1 fail-closed decision tree

Without typed codes, these failures would surface as ad-hoc error strings — operator triage would have no stable identifier to search Loki / Telegram for.

---

## §1. INSTR-FETCH-01 — CSV fetch hard-failed (after retry exhaustion)

**Severity:** Critical.
**Auto-triage:** No (operator escalation required).
**Trigger:** Sub-PR #3's `CsvDownloader::fetch_csv()` returned `Err(_)` AND the §4 retry policy ladder has reached the Critical escalation tier (attempt 11+ within the trading-day window).

**Triage steps:**
1. `mcp__tickvault-logs__tail_errors` — look for `code = "INSTR-FETCH-01"` to read the underlying `CsvDownloadError` payload.
2. Check Dhan CDN status — `curl -sI https://images.dhan.co/api-data/api-scrip-master-detailed.csv` (HEAD request).
3. If Dhan CDN is up: check our static IP + DNS + reqwest TLS config. Possible §18 hardening regression (redirect, body cap, Content-Type).
4. If Dhan CDN is down: wait — the §4 infinite-retry loop continues automatically; the alert tells you we're STILL retrying.
5. If permanently corrupt CSV (rare): operator override via `--operator-acknowledge-stale-csv <sha256_of_yesterday>` flag per §20.

**Source:** `crates/core/src/instrument/csv_downloader.rs` (Sub-PR #3) emit site lands in Sub-PR #10 orchestrator.

---

## §2. INSTR-FETCH-02 — CSV schema validation failed

**Severity:** Critical.
**Auto-triage:** No.
**Trigger:** Sub-PR #4's `parse_detailed_csv()` returned `Err(...)`. Three sub-causes:
- `CsvParseError::MissingHeaderColumn` — Dhan schema regression (a required column disappeared from the CSV header)
- `CsvParseError::NotUtf8` — Dhan served non-UTF-8 bytes (ISO-8859-1 leak)
- `CsvParseError::RowFailureThresholdExceeded` — more than 0.1% of rows had missing mandatory fields

**Triage steps:**
1. `mcp__tickvault-logs__tail_errors` — read the `failed_rows` / `total_rows` diagnostic in the error payload.
2. Inspect the raw CSV: `curl -s https://images.dhan.co/api-data/api-scrip-master-detailed.csv | head -20` — verify the header columns + first few rows.
3. If header has a NEW column with a different name (e.g. `SECURITYID` instead of `SECURITY_ID`): Dhan schema break — file Dhan support ticket + update Sub-PR #4 parser.
4. If many rows have empty mandatory fields: Dhan data quality issue — Dhan support.
5. **Operator override via `--operator-acknowledge-stale-csv` is also valid here** — use yesterday's CSV until Dhan ships a fix.

**Source:** `crates/core/src/instrument/csv_parser.rs` (Sub-PR #4) emit site lands in Sub-PR #10 orchestrator.

---

## §3. INSTR-FETCH-03 — F&O dangling-reference threshold exceeded

**Severity:** Critical.
**Auto-triage:** No.
**Trigger:** Sub-PR #5's `extract_fno_underlyings()` returned `Err(ExtractError::DanglingReferenceThresholdExceeded)`. More than 0.5% of FUTSTK/OPTSTK rows referenced an `UNDERLYING_SECURITY_ID` that did NOT resolve to any NSE_EQ row in the same CSV.

**Triage steps:**
1. `mcp__tickvault-logs__tail_errors` — read `dangling_count / total_derivative_count` diagnostic.
2. Spot-check the affected stocks: query the CSV for FUTSTK rows with the dangling underlying SIDs.
3. Possible causes:
   - Stock delisted but F&O contracts still listed (NSE administrative timing — rare)
   - Dhan CSV consistency bug (the NSE_EQ row was dropped accidentally)
   - Our parser regression (Sub-PR #4 dropped good rows below the 0.1% threshold)
4. If <1% dangling: likely Dhan-side timing — retry on next trading day's CSV is typically fine.
5. If >10% dangling: serious Dhan-side issue — file Dhan support ticket.

**Source:** `crates/core/src/instrument/fno_underlying_extractor.rs` (Sub-PR #5) emit site lands in Sub-PR #10 orchestrator.

---

## §4. INSTR-FETCH-04 — Universe-size envelope violated

**Severity:** Critical.
**Auto-triage:** No.
**Trigger:** Sub-PR #7's `build_daily_universe()` returned `Err(BuildError::UniverseSizeOutOfBounds)`. Computed size is outside `[MIN_DAILY_UNIVERSE_SIZE = 100, MAX_DAILY_UNIVERSE_SIZE = 1200]`.

**Triage steps:**
1. `mcp__tickvault-logs__tail_errors` — read `actual / min / max` diagnostic.
2. **Below 100**: upstream extractor (Sub-PR #5 or #6) returned partial data. Re-run with `--dry-run-universe` to inspect what each extractor contributed. Possible causes:
   - Sub-PR #5 dropped most derivatives via the 0.1% threshold (cascade from INSTR-FETCH-02)
   - Sub-PR #6 found <30 NSE indices (Dhan CSV regression)
3. **Above 400**: upstream regression let in extra rows. Possible causes:
   - BSE non-SENSEX indices leaked through Sub-PR #6's filter (rule-file violation)
   - Currency/commodity F&O rows leaked through Sub-PR #5's filter
   - Dhan added new sectoral indices that pushed past the envelope (legitimate growth — update `MAX_DAILY_UNIVERSE_SIZE` constant via rule-file edit)
4. Operator must decide: relax the envelope (update Sub-PR #2 constants + rule file §22) OR fix the upstream extractor regression.

**Source:** `crates/core/src/instrument/daily_universe.rs` (Sub-PR #7) emit site lands in Sub-PR #10 orchestrator.

---

## §5. Cross-reference

| Component | File | Sub-PR |
|---|---|---|
| `ErrorCode::InstrFetch01CsvHardFailed` | `crates/common/src/error_code.rs` | #9 (this PR) |
| `ErrorCode::InstrFetch02SchemaValidationFailed` | `crates/common/src/error_code.rs` | #9 |
| `ErrorCode::InstrFetch03DanglingReferences` | `crates/common/src/error_code.rs` | #9 |
| `ErrorCode::InstrFetch04UniverseSizeOutOfBounds` | `crates/common/src/error_code.rs` | #9 |
| Production emit sites | `crates/app/src/main.rs` (boot orchestrator) | #10 |
| `instrument_fetch_audit` table + persistence | `crates/storage/src/instrument_fetch_audit_persistence.rs` | #9b (follow-on) |
| `instrument_lifecycle` table | `crates/storage/src/instrument_lifecycle_persistence.rs` | #9b |
| CloudWatch alarm + SNS routing | `deploy/aws/cloudwatch-alarms/instr-fetch-alarms.tf` | #12 |

---

## §6. Why this is a contract-stub PR (#9)

Per the operator's "one PR at a time + small focused changes" preference, Sub-PR #9 ships ONLY the error code enum variants + this runbook. The table schemas + persistence + production emit sites land in:
- **Sub-PR #9b** — `instrument_lifecycle` + `instrument_lifecycle_audit` + `instrument_fetch_audit` tables (DDL + persistence module)
- **Sub-PR #10** — Boot orchestrator that consumes Sub-PRs #3-#7 + emits these codes

This sequencing means the cross-ref test (`error_code_rule_file_crossref.rs`) passes for Sub-PR #9 alone (variants mentioned in this file → cross-ref satisfied), and downstream code-touching sub-PRs `use` the stable identifiers without circular dependencies.

---

## §7. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (adding / modifying any `InstrFetch*` variant)
- Future `crates/app/src/main.rs` boot orchestrator Step 6c (Sub-PR #10)
- Future `crates/storage/src/instrument_lifecycle_persistence.rs` (Sub-PR #9b)
- Future `crates/storage/src/instrument_fetch_audit_persistence.rs` (Sub-PR #9b)
- Any file containing `INSTR-FETCH-` or `InstrFetch0`
