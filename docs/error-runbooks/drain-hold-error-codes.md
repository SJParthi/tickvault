# Drain hold — error codes

> **Authority:** operator Quote 31 (2026-10-21), recorded in
> `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`.
> **Code:** `DRAIN-HOLD-01` (`ErrorCode::DrainHold01NotWritten`), Severity::Medium,
> log-sink-only (no page, per Quote 31).

## What the drain hold is

After the close the app keeps the box running until the day's data is in
the database: every skipped order-book bucket written back, every spill
folder drained, and QuestDB's WAL apply backlog back to zero. While work is
left it writes `/tickvault/<env>/drain-hold` in SSM, refreshed every five
minutes with an expiry 20 minutes ahead. The 17:30 daily stop, the 17:45
stop check, the hourly curfew check and the hard-stop guard's out-of-window
stop all leave the box running while that hold is fresh. The budget stop is
never held. No hold can keep the box past 07:30 IST the next morning.

## DRAIN-HOLD-01 — the hold could not be written

**Meaning:** an SSM `PutParameter` for the hold failed (timeout, throttling,
permission). The previous hold keeps the box up until it expires; after that
the scheduled stops stop the box even though work is left.

**Data at risk:** none is lost. Skipped depth stays marked and its WAL
segments stay on disk; spill files stay in their folders; QuestDB keeps
applying its backlog on the next boot. The day simply does not finish the
same day, and the next morning boots with that work still waiting.

**Triage:**
1. Read the `source` and `error` fields of the log line.
2. Check the instance role still grants `ssm:PutParameter` on
   `/tickvault/<env>/*` (`deploy/aws/terraform/main.tf`, the instance policy).
3. Check the parameter by hand:
   `aws ssm get-parameter --name /tickvault/prod/drain-hold`.
4. If the box was stopped with work left, nothing needs doing: the next
   evening's after-close work picks it up.

**`source = "evening_drain_state"` (2026-10-21, plan ITEM 60):** the evening
drain's state file (`data/state/evening-drain.state`) could not be written.
If it was the restart record, no planned restart is made that minute (an
unrecorded restart would not count toward the evening's cap of three); the
next minute tries again. If it was the pending flag, the restarted boot's
longer catch-up clock may apply once more, until the flag ages out after
15 minutes. Nothing is lost: the WAL segments stay on disk for the next
restart or the next boot. Check the disk is not full and `data/state/` is
writable.

## Related

- The evening drain: `crates/app/src/evening_drain.rs`.
- The write-back pass: `crates/app/src/dhan_feed_stack/deferred_depth_pass.rs`.
- The hold: `crates/app/src/drain_hold.rs`.
- The readers: `crates/aws-lambdas/src/drain_hold.rs`, used by the start
  watchdog and the hard-stop guard.
