# Option Chain + Cross-Verify Error Codes (RETIRED)

> **⚠ RETIRED 2026-06-28.** The 8 `OPTION-CHAIN-01..08` `ErrorCode` variants and the entire `option_chain` REST subsystem were DELETED per operator directive 2026-06-28 ("drop the option chain entire implementations and its table also"). The subsystem was disabled since 2026-06-02 (no Dhan Option Chain Data API entitlement) with no live consumer; its QuestDB table was dropped 2026-06-23. The §1 historical content below is retained for audit only — NONE of these codes exist as enum variants anymore. (The §2 CROSS-VERIFY codes were already retired 2026-05-26.)
>
> **Status:** CONTRACT STUBS (historical). Production emit sites land in PR #8 (option_chain module) and PR #9 (cross_verify module) of the AWS-lifecycle 14-PR sequence per `.claude/plans/aws-lifecycle/THE-FINAL-PLAN.md` §5.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion docs:**
> - `docs/architecture/option-chain-z-plus-heart-piece.md` §8 — 8 OPTION-CHAIN codes
> - `docs/architecture/aws-indices-only-locked-architecture.md` §14.4 — 4 CROSS-VERIFY codes
> - `docs/architecture/dhan-api-coverage-map.md` — Dhan v2.5 compliance
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires each variant to be mentioned in at least one rule file. This file serves that contract for the 12 variants below.

---

## §0. Why these codes exist

When the option_chain REST fetch loop (every 50s for NIFTY/BANKNIFTY/SENSEX) and the cross_verify daily gates (15:31 IST same-day + 08:05 IST morning 1d) land in PRs #8 and #9, those subsystems will emit `error!`/`warn!`/`info!` macros with `code = ErrorCode::X.code_str()` fields. The pre-merge tag-guard (`crates/common/tests/error_code_tag_guard.rs`) rejects any emit without a matching variant. Stubbing the variants in PR #1 means PRs #8 and #9 can compile cleanly without circular dependencies.

---

## §1. Option Chain codes (8 variants — Z+ heart-piece) — RETIRED 2026-06-28

> **RETIRED 2026-06-28** with the deleted `option_chain` REST subsystem. The
> `OptionChain01..08` `ErrorCode` variants no longer exist. The descriptions
> below are historical only.

Per `option-chain-z-plus-heart-piece.md` §8. The option_chain REST fetcher runs every 50s for 3 underlyings concurrently, populates a RAM cache, async-flushes to QuestDB. Strategy fail-closes if cache age > 60s.

### OPTION-CHAIN-01 — REST fetch failed (network timeout / 5xx)

**Severity:** High.
**Auto-triage:** Yes (transient).
**Trigger:** `reqwest::post("/v2/optionchain")` returned non-2xx or timed out. The fetch retries ONCE with 2s delay; if still failing, the underlying is marked STALE for this cycle. RAM cache keeps the previous snapshot; strategy fail-closes if `cache_age_seconds > 60`.
**Triage:** transient — next 50s cycle retries. Sustained failure → cross-check `/v2/profile` reachability and Dhan status.

### OPTION-CHAIN-02 — DH-904 backoff ladder exhausted

**Severity:** High.
**Auto-triage:** Yes (will retry on next non-backoff window).
**Trigger:** Dhan rate-limited us. Backoff ladder 10→20→40→80s per `dhan-annexure-enums.md` rule 11. After 80s exhaustion, the cycle is skipped and the alarm fires.
**Triage:** confirm we're not exceeding 1 unique request per 3s per underlying. Check `tv_option_chain_cycle_overlap_total` counter.

### OPTION-CHAIN-03 — response parse failure

**Severity:** Medium.
**Auto-triage:** Yes (retry once).
**Trigger:** Dhan returned 2xx but body is malformed JSON, missing `oc` map, or field types disagree with `dhan/option-chain.md` schema. Retry once with same expiry; mark stale on repeat. Possible Dhan-side API regression.
**Triage:** compare raw body in `option_chain_raw` table to expected schema. Open Dhan support ticket if persistent.

### OPTION-CHAIN-04 — LTP disagrees with WS LTP

**Severity:** Medium.
**Auto-triage:** No (data integrity).
**Trigger:** option-chain `data.last_price` of the underlying disagrees with the live WebSocket index LTP beyond 0.5% tolerance. Either our parser is wrong or Dhan-side data is anomalous. Operator inspection required.

### OPTION-CHAIN-05 — cache stale, strategy halted

**Severity:** Critical.
**Auto-triage:** No (operator paged via 4-channel SNS).
**Trigger:** `option_chain_cache.age_seconds(underlying) > 60` during market hours. Strategy refuses to emit any signal. Operator paged via SMS + Telegram + Email + Amazon Connect outbound voice CALL.
**Triage:** open `option_chain_request_audit` table to see latency + failure history for the last 5 minutes. Likely upstream Dhan REST is slow or down.

### OPTION-CHAIN-06 — cycle overlap, skip-next

**Severity:** High.
**Auto-triage:** Yes (mutex skips next 50s window).
**Trigger:** previous cycle still running when next 50s tick fires. Mutex-guarded skip-next-cycle policy. If 2 skips in a row, alarm escalates.
**Triage:** check per-call latency in `option_chain_request_audit`. If a single underlying is consistently slow, isolate via per-underlying alarm threshold.

### OPTION-CHAIN-07 — Thursday expiry rollover detected

**Severity:** Info.
**Auto-triage:** Yes (informational).
**Trigger:** at 15:31 IST on Thursday (or whenever NSE expiry rolls), the expiry-list fetch returns a different nearest expiry than was active during the day. RAM cache is rebuilt for the new expiry; next 50s cycle uses the new expiry. No action required.

### OPTION-CHAIN-08 — JWT expired mid-cycle (HTTP 401)

**Severity:** Critical.
**Auto-triage:** No.
**Trigger:** HTTP 401 from Dhan mid-cycle indicates the 24h JWT expired between cycles. The token-manager force-refresh fires, but if the token can't be refreshed (TOTP secret rotated, SSM unreachable), the strategy cannot operate. Operator paged.
**Triage:** verify SSM `/tickvault/prod/dhan_totp_secret` matches Dhan web UI. Re-register TOTP if rotated.

---

## §2. Cross-Verify codes — RETIRED (PR-C 2026-05-26)

The 4 CROSS-VERIFY codes (CROSS-VERIFY-01..04) and their entire
`cross_verify` chain were deleted in PR-C of the Dhan historical fetch
chain removal series (2026-05-26). Operator directive narrowed the
runtime to spot-only NIFTY 50 strategy; no VWAP, no futures, no
historical fetch, no L3 RECONCILE pass.

Deleted artefacts: `cross_verify.rs`, `cross_verify_report.rs`,
`cross_verify_scheduler.rs`, `post_open_cross_check.rs`,
`post_market_fetch_window.rs`, `crates/core/src/cross_verify/`,
7 guard tests, 4 `ErrorCode` variants, 3 `NotificationEvent` variants,
5 `CROSS_VERIFY_*` constants.

---

## §3. Mechanical guarantee mapping (operator-charter §C)

| Charter demand | How these stubs satisfy |
|---|---|
| 100% audit coverage | Each code's full forensic detail is the Telegram alert + the structured `error!` line in `data/logs/errors.jsonl.*` (hourly-rotated, 48h retained). Cross-verify is table-free per #T5 — no QuestDB audit table |
| 100% logging | Each code carries a `Severity` so tracing macros route correctly to CloudWatch |
| 100% alerting | Critical-severity codes auto-route to SNS 4-leg fan-out (SMS + TG + Email + Connect call) |
| 100% extreme check | `error_code_tag_guard.rs` enforces `code = ErrorCode::X.code_str()` field on every emit; `error_code_rule_file_crossref.rs` enforces this file's existence + cross-references |

---

## §4. Honest envelope (operator-charter §F)

> "PR #1 ships ONLY contract stubs — no consumer code, no emit sites. The 12 variants exist so PRs #8 (option_chain module) and #9 (cross_verify module) can compile their emit calls against stable identifiers when they land in 2-4 weeks. Until those PRs merge, ZERO production code emits these codes. This is intentional per the 14-PR sequence in `THE-FINAL-PLAN.md` §5 — cement contracts first, then add the implementations behind them."

---

## §5. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (when adding/modifying any `OptionChain*` or `CrossVerify*` variant)
- Future `crates/core/src/option_chain/*.rs` (when PR #8 lands)
- Future `crates/core/src/cross_verify/*.rs` (when PR #9 lands)
- `docs/architecture/option-chain-z-plus-heart-piece.md`
- `docs/architecture/aws-indices-only-locked-architecture.md` §14
- Any file containing `OPTION-CHAIN-` or `CROSS-VERIFY-` or `OptionChain0` or `CrossVerify0`
