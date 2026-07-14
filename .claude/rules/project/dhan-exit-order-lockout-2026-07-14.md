# 🔷 DHAN Exit-Order Layer Lockout — Error Codes (EXIT-ORDER-01 / EXIT-VERIFY-01)

> **PLACEHOLDER (WP1, 2026-07-14):** full runbook — the 4-lock table, the
> enable-time protocol, the REJECT list, the guard-test list, the honest
> Forever/OCO scope note, and the live-probe ledger — lands with WP5 in
> this same PR. This stub exists so the error-code cross-ref tests hold
> both directions from the first commit.

## EXIT-ORDER-01 — exit engine call degraded

`ErrorCode::ExitOrder01ExecutionDegraded` (`code_str() == "EXIT-ORDER-01"`).
Severity::High, auto-triage-safe YES, **log-sink-only** (no
`error_code_alerts` entry — the 2026-07-14 Dhan noise lock posture).
Emitted on: an exit engine call failing (validation refusal on a
dispatched command, Dhan API error, ENTRY_LEG cancel refused post-fill,
slicing response anomaly). Full triage: WP5.

## EXIT-VERIFY-01 — MPP verify ladder exhausted degraded

`ErrorCode::ExitVerify01Degraded` (`code_str() == "EXIT-VERIFY-01"`).
Severity::High, auto-triage-safe YES, **log-sink-only**. Emitted on: the
verify ladder exhausting with `PendingAtLimit` (MPP MARKET→LIMIT resting
past the deadline — never assumed filled), a partial fill at budget
(`needs_reconciliation` set — the remainder is never silently forgotten),
or an `Unknown` unparsable status (fail-closed). Full triage: WP5.
