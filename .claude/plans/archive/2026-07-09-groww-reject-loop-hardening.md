# Implementation Plan: Groww reject-loop hardening — never-streamed restart arm + FEED-REJECT-01 cause surfacing

**Status:** APPROVED
**Date:** 2026-07-09
**Approved by:** Parthiban via coordinator escalation 2026-07-09, quote: "design + ship the hardening PR TODAY"

> Guarantee matrices: this plan cross-references the canonical 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` (as that rule
> explicitly allows). Per-item proof: no new tick-drop path, cold-path
> (supervisor) only, every new pub fn tested + wired, ratchet tests added,
> `error!` lines carry `code =` fields.

## Context (the 2026-07-09 incident)

The Groww live feed paged `[HIGH] Groww live feed rejected — the feed reported
an error and is retrying; connected but receiving nothing` at 09:22 IST AND
again at 14:17 IST — an all-day recurring loop the FEED-STALL-01 30s
stall-restart did not cure. That exact Telegram wording maps to
`SidecarLineClass::Error` only (`classify_sidecar_line`,
`crates/app/src/groww_sidecar_supervisor.rs`). Two verified residuals:

1. `should_restart_on_stall` requires a KNOWN last-tick
   (`last_tick_age_secs == None → false`), so a feed that NEVER streamed its
   first tick this session is NEVER stall-restarted — the "connected +
   subscribed but 0 ticks forever" hole.
2. The alert reason is a FIXED string per class (correct for Telegram per the
   10 commandments), but NO coded, bounded, sanitized capture of the
   triggering sidecar line reaches the error stream — so errors.jsonl /
   CloudWatch cannot answer WHY the loop repeats without scrolling raw
   per-line logs.

Gap analysis: no merge since 4c9cc2d (#1442/#1444/#1445/#1447/#1449/#1450)
touches `groww_sidecar_supervisor.rs`, `groww_sidecar.py`, or `feed_health.rs`
(verified via `git log 4c9cc2d..origin/main -- <paths>` = empty).

## Design

Touched crates: **tickvault-app** (`crates/app/src/groww_sidecar_supervisor.rs`,
`crates/app/src/main.rs`), **tickvault-common**
(`crates/common/src/error_code.rs`). Rule file:
`.claude/rules/project/feed-stall-watchdog-error-codes.md`.

### D1 — Never-streamed-first-tick restart arm (closes residual 1)

- New constant `FEED_NEVER_STREAMED_RESTART_SECS: u64 = 300` (5 min — upper
  end of the 3–5 min band, chosen so the earliest possible fire is 09:20:05
  IST, safely past any slow session start).
- New pure decision `should_restart_on_never_streamed(silent_session_secs:
  Option<u64>, in_exchange_session: bool, enabled: bool, threshold_secs: u64)
  -> bool` — mirrors `should_restart_on_stall` shape (strict `>`, fail-closed
  on `None`, feed-agnostic).
- `supervise_child` gains a per-child `never_streamed_open_since: Option<u64>`
  window tracker inside the EXISTING stall poll arm:
  - `age.is_some()` (first tick arrived this session) → window cleared forever
    (the existing stall arm owns liveness from then on).
  - Gate = `g1_exchange_gate_accepts(now_ist_nanos_of_day)` ([09:15, 15:30)
    IST) AND `is_trading_session_now()` (weekday + NSE-holiday aware) AND
    `enabled`. Outside the gate the window clears — pre-open (Dhan-style 09:00
    persist window), weekends, and holidays can NEVER arm it (the plain
    `is_within_market_hours_ist` gate the stall arm uses is time-of-day only;
    using it here would have restart-looped every Saturday 09:00–15:30).
  - Fire → same kill+relaunch as the stall arm: shared `StallRestartStorm`
    record, `warn!(code = FEED-STALL-01, reason = "never_streamed")` (storm
    edge → `error!`, matching the existing level split so the FEED-STALL-01
    error-code alarm semantics are unchanged), increment the EXISTING
    pre-registered `tv_feed_sidecar_stall_restart_total{feed}` (so the
    ≥3-per-15-min restart pager fires on a persistent never-streamed loop —
    the operator's "feed keeps failing after restarts" signal, making the
    optional escalation Telegram redundant) PLUS a NEW attribution counter
    `tv_feed_sidecar_never_streamed_restart_total{feed}`, then return
    `SuperviseOutcome::StallRestart` (storm → `Exited`).
- Each fresh child resets the window, so every relaunched sidecar gets the
  full 300s to produce its first tick (bounded ~5-min restart cadence; can
  never fight the existing backoff/stall arm — the arm only runs while
  `age == None`, the stall arm only while `age == Some`; disable toggle still
  wins via the existing re-read).
- `crates/app/src/main.rs`: pre-register the new counter `.increment(0)`
  immediately next to the existing stall-restart pre-registration (same
  CW-agent delta-baseline rationale).

### D2 — FEED-REJECT-01 bounded cause signature (closes residual 2)

- New ErrorCode variant `FeedReject01SidecarErrorDetail` → code_str
  `"FEED-REJECT-01"`, Severity::High, auto-triage-safe (High default),
  runbook `.claude/rules/project/feed-stall-watchdog-error-codes.md`.
- New pure fn `sidecar_line_signature(line: &str) -> String`: pipes the
  (already sidecar-redacted) line through the existing
  `tickvault_common::sanitize::capture_rest_error_body` (control-char strip →
  URL/credential/JWT redaction → 300-char cap) then truncates to
  `SIDECAR_LINE_SIGNATURE_MAX_CHARS = 160` chars (UTF-8-safe `chars().take`).
- `spawn_pipe_drain`: inside the EXISTING once-per-child alert edge
  (`!alerted.swap(true)`), emit ONE
  `error!(code = "FEED-REJECT-01", feed, class, signature = %…)` carrying the
  bounded signature — BEFORE the Telegram fan-out, unconditional of the
  notifier slot. Telegram wording is UNCHANGED (fixed per-class reason; the
  raw child text still never reaches Telegram). Bounded: ≤1 per child episode
  on the single-conn path; ≤1 per 60s fleet window per child on the fleet
  path (the existing Suppress re-arm).

### D3 — item 3 (escalation Telegram) deliberately SKIPPED

The counter-based `tv-<env>-feed-stall-restarts` pager (Sum ≥ 3 / 15 min)
already delivers exactly the "feed keeps failing after restarts — needs
manual check" page, and D1 now routes never-streamed restarts into that same
counter. A new NotificationEvent + episode latch would duplicate that signal
for real diff cost. Recorded as a conscious scope decision.

## Edge Cases

- **Pre-open / weekend / holiday**: gate = exchange session [09:15, 15:30) ∧
  trading-day (`is_trading_session_now`, calendar-aware) — window clears
  outside it; a Saturday inside the 09:00–15:30 clock window can never arm.
- **Slow first tick at open**: earliest fire 09:20:05 IST (300s after the
  09:15:00 gate opens) — a healthy feed streams within seconds of open across
  ~770 SIDs.
- **Exactly at threshold**: strict `>` — 300s exactly does NOT fire (test).
- **First tick arrives mid-window**: `age.is_some()` clears the window
  permanently for the session; the stall arm owns liveness thereafter (no
  double-owner, no double restart).
- **Disable toggle mid-window**: `enabled` re-read inside the arm; the
  disable poll arm still wins (unchanged); the window also clears.
- **Backward clock step**: `saturating_sub` on the window math; the shared
  `StallRestartStorm` already tolerates backward steps.
- **Storm interplay**: never-streamed restarts record into the SAME storm
  window as stall restarts; a mixed flap escalates exactly as today.
- **Fleet path**: the never-streamed arm runs per-child inside
  `supervise_child` (fleet children each have their own window); dormant on
  `main` (scale lockout, `enabled=false` fleet never spawns).
- **Signature content**: multi-byte UTF-8 truncation is char-boundary-safe;
  control chars stripped (no newline injection into the one-line log field);
  JWT-shaped substrings redacted even if the sidecar's own redaction ever
  misses one.

### Post-impl adversarial review outcomes (2026-07-09, folded in)

- **HIGH (fixed):** page-storm amplification — the ~5-min never-streamed
  relaunch cadence × a fresh per-child `alerted` latch would have paged the
  GrowwSidecarRejected HIGH ~12×/hour. Fixed with a supervisor-lifetime
  cross-child page cooldown (`GROWW_REJECT_PAGE_COOLDOWN_SECS = 1800`, pure
  `should_page_reject`, CAS'd across the two drains) on the single-conn
  Passthrough arm; suppressed pages counted by
  `tv_groww_reject_page_cooldown_suppressed_total`; all other side-effects
  (error! forwards, FEED-REJECT-01, feed-health) unchanged.
- **MEDIUM (fixed):** the fleet Suppress re-arm of `alerted` would have made
  the FEED-REJECT-01 emit per-line on the fleet path — it now has its OWN
  per-child `detail_logged` latch, re-armed only on streaming recovery.
- **MEDIUM (documented, runbook §1b):** the 300s relaunch resets the
  sidecar's 600s "access token stale" escalation clock in-session; the
  per-cycle `error [auth]` AuthRejected class + FEED-REJECT-01 signatures
  carry the same diagnosis.
- **MEDIUM (documented, runbook §1b):** +12 Groww sessions/hour churn is
  marginal vs the sidecar's internal reconnect ladder (up to ~720/hour).
- **Security LOWs (2 fixed, 1 flagged):** `auth_token` added to the
  credential-field redaction keys in `capture_rest_error_body`;
  BiDi/zero-width/BOM strip added to `sidecar_line_signature`; AWS
  `x-amz-security-token` level-suppression-only protection on the Python
  side is pre-existing and flagged as follow-up.
- **LOWs (comments):** G1-gate-as-wall-clock-window note at the call site;
  "session" = app process wording corrected.

## Failure Modes

- **Sidecar that can never stream (entitlement/server-side)**: bounded ~5-min
  restart loop during session hours; the restart pager fires after 3
  restarts; each episode's FEED-REJECT-01 signature names the cause. Honest:
  the restart cannot force Groww's server to send data.
- **kill/wait failure on restart**: same best-effort `start_kill` + `wait`
  path as the stall arm (warn + proceed).
- **capture_rest_error_body regression**: ratcheted in common; the signature
  fn adds only a shorter cap on top.
- **Counter emission before recorder install**: same benign no-op residual as
  the existing counter (pre-registration in main.rs covers the delta
  baseline).
- **FEED-REJECT-01 has NO CloudWatch alarm** (log-sink-only by design today;
  the page comes from the existing GrowwSidecarRejected Telegram). Adding a
  `tv-<env>-errcode-feed-reject-01` filter is a flagged follow-up — one
  `error_code_alerts` map entry in `deploy/aws/terraform/error-code-alarms.tf`.

## Test Plan

- `cargo test -p tickvault-app` (touched crate) — new unit tests:
  - `should_restart_on_never_streamed`: fires past threshold in-session;
    strict boundary; false on `None`; false out-of-session; false disabled.
  - `sidecar_line_signature`: 160-char cap (UTF-8 boundary), JWT redaction,
    control-char strip, clean short line passes through.
  - constants sanity: never-streamed threshold > stall threshold and inside
    [180, 600].
  - source-scan ratchets: supervise_child wires the never-streamed decision +
    both counter increments; spawn_pipe_drain emits the FEED-REJECT-01 coded
    signature inside the alert edge; main.rs pre-registers the new counter.
- `cargo test -p tickvault-common` — error_code catalogue tests (all/roundtrip/
  prefix/severity/runbook) + `error_code_rule_file_crossref` +
  `error_code_tag_guard` (workspace escalation per testing-scope.md is
  satisfied by CI's full battery; locally common+app are the signal).
- Hooks: banned-pattern scanner, plan-verify, per-item-guarantee-check.

## Rollback

Single squash-merge revert (`git revert <merge-sha>`) restores prior
behaviour byte-identically: the new arm, counter, ErrorCode variant, and
signature emit are additive; no schema, no config, no Dhan-path, no hot-path
change. No data migration. The ErrorCode variant removal on revert is safe
(crossref rule-file section notes retirement convention).

## Observability

- `tv_feed_sidecar_stall_restart_total{feed}` — UNCHANGED series, now also
  incremented by never-streamed restarts → the existing
  `tv-<env>-feed-stall-restarts` pager covers the new failure mode.
- NEW `tv_feed_sidecar_never_streamed_restart_total{feed}` (static labels,
  pre-registered) — attribution split.
- `warn!/error!(code = FEED-STALL-01, reason = "never_streamed")` — same
  level split as the stall arm (per-restart warn!, storm error!) so the
  FEED-STALL-01 error-code alarm semantics are unchanged.
- NEW `error!(code = FEED-REJECT-01, class, signature)` — one bounded,
  secret-redacted cause line per reject episode in errors.jsonl → CloudWatch.
- Runbook: dated 2026-07-09 section in
  `.claude/rules/project/feed-stall-watchdog-error-codes.md` (crossref target
  for the new variant).
- Telegram: UNCHANGED wording (10 commandments hold).

## Plan Items

- [x] D1 never-streamed restart arm
  - Files: crates/app/src/groww_sidecar_supervisor.rs, crates/app/src/main.rs
  - Tests: test_should_restart_on_never_streamed_fires_past_threshold_in_session,
    test_should_restart_on_never_streamed_boundary_and_gates,
    test_never_streamed_constants_sane,
    test_never_streamed_arm_is_wired_into_supervise_child,
    test_never_streamed_counter_is_preregistered_in_main
- [x] D2 FEED-REJECT-01 cause signature
  - Files: crates/common/src/error_code.rs, crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_sidecar_line_signature_caps_redacts_and_strips,
    test_feed_reject_emit_is_wired_into_alert_edge
- [x] Runbook update (dated section)
  - Files: .claude/rules/project/feed-stall-watchdog-error-codes.md
  - Tests: error_code_rule_file_crossref (existing, must stay green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Feed never streams from 09:15 on a trading day | restart at ~09:20:05, then every ~5 min; restart pager fires after 3; FEED-REJECT-01 signatures name the cause |
| 2 | Feed streams at 09:15:03 then dies at 11:00 | never-streamed arm never fires; existing 30s stall arm restarts |
| 3 | Saturday 10:00 IST, 0 ticks | window never arms (trading-day gate) — no restart, no page |
| 4 | Pre-open 09:05 IST, 0 ticks | window never arms (exchange-session gate) |
| 5 | Error-class line with embedded JWT-shaped token | FEED-REJECT-01 signature carries `[REDACTED-JWT]`, ≤160 chars |
| 6 | Operator disables Groww mid-silence | disable arm wins; no restart fight |
