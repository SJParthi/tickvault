# Runtime recovery refusal diagnostics

Added 2026-09-15 for PR #1914. These entries document error semantics and
recovery evidence; they do not change permissions, approval gates or budgets.

| Code | Condition and retained behavior | Investigation |
|---|---|---|
| `WAL-SEQUENCE-01` | Identity allocation, committed capacity, directory ownership or reservation sync is unavailable. Live capture can stop; old IDs must not be reused. | Inspect the ownership and sequence manifest, remaining reserved capacity, filesystem errors and complete migration evidence. Never manufacture, lower or delete the sequence authority to restore traffic. |
| `WAL-RECOVERY-01` | Replay completeness, directory durability, confirmation receipt or archive publication was refused. The error does not establish that all frames are present or durable. | Preserve original WAL and quarantine files. Resolve the named CRC, ownership, receipt or filesystem error; repeat verification before confirming the same generation. |
| `CANDLE-SCHEMA-01` | A required projection or schema migration was refused. Populated or unverified historical tables stay preserved, and required writer prerequisites remain blocking. | Verify actual schema and retained data. Use the reviewed migration and restore procedure; do not infer emptiness from a failed query or drop a populated table to unblock startup. |
| `CANDLE-RANK-01` | Ranking metadata, universe registration or snapshot validation failed, or the current session ranking was invalidated. | Inspect the source/reason and publication generation. A rejected update does not prove a complete or current board. Resolve the underlying metadata/candle condition before reopening decision use. |

All four codes have **High** severity and `is_auto_triage_safe() == false`.
Their stable code fields reach the configured ERROR log sink for operator
inspection. **There is no dedicated CloudWatch paging filter for these four
codes in this change.** The explicit log-sink-only classification records
that limitation; it does not claim automatic notification.

The existing actual-frame-loss path still increments the durable-floor loss
counters and emits `WS-SPILL-02`. Boot completion/liveness alarms can detect
failure to start. These are separate conditions: neither route proves that
every sequence reservation, retained replay, schema or ranking refusal pages.
No metric selector, alarm budget, alarm action or notification routing changes.

Sequence-admission and directory-sync diagnostics use the existing refusal
ladder: first refusal, powers of two, then one line per 1,048,576 failures.
Their counters still increment for each refused operation. Successful sequence
allocation does not touch the diagnostic counters or logging path. The bounded
log rate is not a continuous paging guarantee; inspect the counters and feed
health when diagnosing a stopped capture path.

`tv_api_auth_failed_total` already has a separate route: the configured
Prometheus scrape emits into `/tickvault/prod/metrics`, and
`auth-failed-alarm.tf` extracts that unlabelled counter with its existing
metric filter. The visibility guard verifies the actual filter body and agent
log group. It does not add a new EMF series (which would double count) or
per-request ERROR logs (which would amplify hostile traffic).
