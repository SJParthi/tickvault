# Implementation Plan: Telegram HTML escaping for external-text interpolations

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban, 2026-06-12 — selected Option A via AskUserQuestion: "A — Telegram HTML escaping (Recommended): Implement the named security follow-up from #1089's review: a module-level html_escape() applied to externally-sourced `reason` interpolations in surviving Telegram HTML arms ... full 3-agent review + plan-gate, monitored to merge." This is the named MEDIUM follow-up recorded in `active-plan-phase-b-batch3-depth-events-final.md` line 60.

**Guarantee matrices:** See `per-wave-guarantee-matrix.md` + `zero-loss-guarantee-charter.md` §3 (cross-referenced). This is a single-crate (`core`) security-hardening change at the Telegram render boundary — no new pub fns wired into the hot path, no new tables, no new deps, no schema change.

---

## Design

**Problem (real, verified 2026-06-12):** Telegram sends use `"parse_mode": "HTML"` (`crates/core/src/notification/service.rs:771`). `NotificationEvent::to_message()` interpolates external free-text fields (`reason`, `detail`, `message`, `ip_match_status` — sourced from Dhan REST/WS error bodies and network error strings) directly into HTML message bodies. There is **no `html_escape` helper today** (only a markdown `escape_md` in `summary_writer.rs`). A `reason` containing `<`, `>`, or `&` (e.g. a Dhan error body, a TLS error like `peer's certificate contained <...>`, or `a < b`) makes Telegram's HTML parser return **400 Bad Request** — the operator never sees that alert. The existing `test_*_has_no_unescaped_html_brackets` guards only cover **static** `<`/`>` in literal strings (the 2026-04-28 SLO `(< 0.80)` bug); they do NOT inject external strings, so this class is unguarded.

**Fix (single render-boundary chokepoint — DRY, O(1) per message):**
1. Add a private module-level `fn html_escape(s: &str) -> String` in `events.rs` next to `mask_ip_for_notification`. Escapes the three Telegram-HTML-significant characters in the correct order: `&` → `&amp;` FIRST (so we never double-escape the entities we introduce), then `<` → `&lt;`, then `>` → `&gt;`. (`"` is only significant inside attribute values, which these messages never use, so it is intentionally not escaped — documented in the fn doc.)
2. Apply `html_escape(...)` to every interpolation of an **external free-text** field inside `to_message()` and the `boot_blind` helper. Escaping is applied to the FINAL string at the render site — for the auth arms that already call `redact_url_params(reason)`, escape the redacted result (`html_escape(&redact_url_params(reason))`) so redaction-then-escape compose correctly. The literal `<b>`/`<code>` template tags are NOT escaped (they are trusted structure); only the interpolated variable is escaped.

**External-text arms escaped (free text from Dhan/network/operator-external):** `boot_blind` stuck-feed `reason` (≈L1129); `AuthenticationFailed`, `AuthenticationTransientFailure`, `PreMarketProfileCheckFailed`, `MidSessionProfileInvalidated`, `TokenRenewalFailed`, `CrossVerify1mAborted` (`detail`), `WebSocketDisconnected`, `WebSocketDisconnectedOffHours`, `WebSocketReconnected` (`Option` reason), `OrderUpdateDisconnected`, `InstrumentBuildFailed`, `IpVerificationFailed`, `StaticIpBootCheckFailed` (`ip_match_status` — Dhan-returned), `BarMismatchCrossCheckFailed`, `OrderRejected` (`correlation_id` + `reason`), `RiskHalt`, `OptionChainFetchFailed` (`underlying` + `reason`), `OptionChainCacheFallback` (`underlying`), `OptionChainStaleHalt` (`underlying`), `OptionChainConfigInvalid` (`reason`).

**Deliberately NOT escaped (with reason):**
- `Custom { message }` — operator/internally-constructed trusted text; some callers may intentionally pass `<b>` formatting. Not external-attacker-controlled. Out of the named review scope. (If a future caller passes external text via `Custom`, it must escape at the callsite — noted in the fn doc.)
- Purely numeric / internal-`&'static str` enum fields (`writer`, `signal_kind`, `feed`, `source`, `step`, `limit_type`, `weakest`, `failed` check-name list, `security_id`, `exchange_segment`, all `*_secs`/counts) — cannot carry HTML metacharacters.

**Why the render boundary and not each callsite:** escaping once at `to_message()` (the single place strings become HTML) is O(1) per message, impossible to forget at a new callsite, and keeps the 130+ event emit sites unchanged. This matches the existing redaction model (`redact_url_params` is also applied in `to_message`, not at callsites).

## Edge Cases

- **Double-escape:** prevented by escaping `&` first. `html_escape("&lt;")` → `&amp;lt;` only if the INPUT already contained a literal `&` — which is correct (the input was literal text, not pre-escaped). We never feed already-escaped strings in.
- **Existing bracket guards still pass:** they strip `<b>`/`</b>` then assert no `<`/`>`. We do not escape the literal `<b>` tags, and the variable parts in those tests contain no `<>&`, so output is unchanged → green.
- **Redaction order:** auth arms redact URL params first, then escape — both transforms compose; escaping a redacted string is still correct.
- **`<code>{reason}</code>` arms (OptionChainFetchFailed):** only the inner variable is escaped; the `<code>` wrapper stays literal so Telegram renders monospace correctly.
- **Empty string / all-safe string:** `html_escape("")` = `""`; `html_escape("plain text 5s")` = unchanged (no metacharacters) — zero behaviour change for the common case.
- **Unicode:** `&`/`<`/`>` are single ASCII bytes; multi-byte UTF-8 is untouched. `String::replace` is UTF-8-safe.
- **`WebSocketReconnected` Option reason:** escape inside the `Some(r)` projection; `None` path unchanged.

## Failure Modes

- **A surviving unescaped external interpolation:** caught by the new per-arm adversarial tests that inject `"<script>alert(1)</script> a < b & c"` into each fixed field and assert the raw `<script>`/raw external `<` is absent and `&lt;`/`&gt;`/`&amp;` present.
- **Accidentally escaping a literal `<b>` tag (breaking bold):** caught because the new tests also assert the message still contains the literal `<b>` opening tag.
- **Hidden compile breakage from shadowing:** `cargo build --workspace` + `cargo test -p tickvault-core --features daily_universe_fetcher` before commit (CI-exact invocation).
- **3-agent adversarial review** (hot-path + security + hostile) on the diff before the PR is marked ready, per operator-charter §E.
- **Concurrent main movement:** fetch + rebase origin/main before push; re-verify.

## Test Plan

- New unit tests in `events.rs` `#[cfg(test)]`: `test_html_escape_*` (helper: amp-first ordering, `<`, `>`, `&`, mixed, empty, no-double-escape, plain-passthrough) + one `test_<arm>_escapes_external_reason` per fixed external-text arm asserting (a) raw injected `<script>` / external `<` absent, (b) `&lt;`/`&gt;`/`&amp;` present, (c) literal `<b>` structural tag still present.
- `cargo build --workspace` (green baseline already established this session, 2m20s, exit 0).
- `cargo test -p tickvault-core --features daily_universe_fetcher` — paste real `test result: ok. N passed; 0 failed` lines.
- `cargo fmt --check` + CI-exact `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf` — no NEW warnings in touched file.
- Hook gates: plan-gate (this file satisfies it), banned-pattern, pub-fn-test-guard (new `html_escape` is private + tested), test-count ratchet (count INCREASES — adds tests).

## Rollback

Single squash-merged PR confined to `crates/core/src/notification/events.rs`. `git revert <sha>` restores verbatim. No config/schema/data/dep changes; no migration. Reverting cannot lose data — it only re-exposes the (pre-existing) unescaped-render behaviour.

## Observability

- No new metric/log/Telegram event is added; this HARDENS existing Telegram delivery so alerts containing `<`/`>`/`&` in their reason are now actually delivered instead of 400-rejected. Net positive observability — previously-dropped CRITICAL/HIGH pages (auth failure, WS disconnect, order rejection, risk halt with a metacharacter in the reason) now reach the operator.
- The fix is at the render boundary, so EVERY current and future event that interpolates these fields benefits without per-event wiring.
- No counter/label changes → `metrics_catalog`, `dedup_segment_meta_guard`, `error_code_*` ratchets unaffected.

## Plan Items

- [x] Add `html_escape()` helper + escape all external-text interpolations in `to_message()` + `boot_blind`; add adversarial tests
  - Files: crates/core/src/notification/events.rs
  - Tests (12 added, all green): test_html_escape_amp_first_no_double_escape, test_html_escape_plain_passthrough_and_empty, test_websocket_disconnected_escapes_external_reason, test_websocket_disconnected_offhours_escapes_external_reason, test_order_update_disconnected_escapes_external_reason, test_order_rejected_escapes_external_reason_and_correlation, test_risk_halt_escapes_external_reason, test_authentication_failed_escapes_external_reason, test_mid_session_profile_invalidated_escapes_external_reason, test_option_chain_fetch_failed_escapes_external_reason, test_cross_verify_1m_aborted_escapes_external_detail, test_static_ip_boot_check_failed_escapes_ip_match_status
  - Evidence: `cargo test -p tickvault-core --features daily_universe_fetcher --lib` → `test result: ok. 2368 passed; 0 failed`. `cargo fmt --check` exit 0. CI-exact `cargo clippy --workspace --no-deps` → 1 pre-existing warning (constituent_resolver type_complexity, behind daily_universe_fetcher, untouched), ZERO new warnings from events.rs. Baseline `cargo build --workspace` exit 0 (2m20s).

- [x] Security-review follow-on (PR #1102, 3-agent review): escape 3 MEDIUM external-text arms the first sweep missed + 2 flagged String LOWs
  - Findings: hostile bug-hunt → NO CRITICAL/HIGH. security-reviewer → NO CRITICAL/HIGH; html_escape impl confirmed correct + complete; 3 MEDIUM (DualInstanceDetected holder/lock_key from Valkey/SSM lock; OrphanPositionDetected sample_symbols from Dhan `/v2/positions`; BarMismatchCorrectedFromHistorical sample_symbols from Dhan historical) + 4 LOW.
  - Action: fixed all 3 MEDIUM + 2 String LOWs (BootDeadlineMissed `step`, BootClockSkewExceeded `source`). `weakest` is `&'static str` (compile-time-internal → no escape needed, LOW is a type-false-positive); `Custom { message }` deliberately operator-trusted (documented). Added 3 adversarial tests.
  - Files: crates/core/src/notification/events.rs
  - Tests: test_dual_instance_detected_escapes_external_holder_and_lock_key, test_orphan_position_detected_escapes_external_sample_symbols, test_bar_mismatch_corrected_escapes_external_sample_symbols
  - Evidence: 145 events tests pass (+3); CI-exact clippy → 0 new warnings.
