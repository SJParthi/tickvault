# Implementation Plan: Spot Cross-Check Severity Gating + Honest Wording + Pre-Boot Gap Dedup

**Status:** IN_PROGRESS
**Date:** 2026-07-17
**Approved by:** Parthiban (operator, via coordinator — Fix E spec, 2026-07-17)

## Design

Operator-approved Fix E overhaul across `crates/app` + `crates/core`:

1. **`crates/app/src/spot_crossverify_boot.rs`** — the 0-paise exact COMPARE
   stays untouched (§37 doctrine). The per-feed day query moves to the house
   LIMIT+1 truncation probe: query `LIMIT cap+1`, `truncated` only when
   `rows.len() > cap` (a healthy 4-index day is exactly 1,500 rows — the old
   `>= limit` check false-flagged PARTIAL at zero headroom). New severity
   gating: a run is operator-page-worthy (High) ONLY when (a) an OPEN or
   CLOSE field diverged, (b) any single delta exceeds the new
   `[spot_crossverify] noise_threshold_paise` knob (serde default 2000 paise
   = ₹20 — index-level cross-broker sampling skew on 20,000–85,000-point
   indices routinely reaches a few rupees on high/low; ₹20 is ~0.02-0.1% of
   index value, far above observed timing noise yet far below any real feed
   drift), or (c) minutes are missing on one broker. High/low-only skew
   within the threshold = Info with a trend line. COUNTS stay exact — only
   severity + wording change; the audit tables are untouched.
2. **`crates/core/src/notification/events.rs`** — `SpotCrossverifySummary`
   gains `noise_only: bool` + `noise_max_paise: i64`; render rewritten per
   the 10 Telegram commandments (plain verdict lead, broker named, rupees
   not paise, ≤3 example lines, plain-English coverage notes, the honest
   frame "Neither broker is the single source of truth — watch the trend
   over days, not one number." on every arm). Severity fold: clean /
   no_data / noise_only → Info; everything else High.
3. **`crates/app/src/groww_spot_1m_boot.rs`** — before writing
   `named_gap`/`pre_boot` audit rows for the pre-boot window
   [session_first, first_covered), the sweep reads `spot_1m_rest` for that
   (feed='groww', SID, day) window (bounded LIMIT+1, cap 400/SID) and
   EXCLUDES minutes a predecessor process already persisted. Read failure or
   truncation fails OPEN to the old name-everything behavior + one coded
   warn (a failed dedup read must never hide real gaps). Kills the
   1088-artifact class.
4. **`crates/app/src/feed_scoreboard_boot.rs` + events.rs scorecard** — the
   rest-leg digest SQL/parse/aggregate gains `error_class` so `pre_boot`
   named-gap rows split into their own `pre_boot_gaps` bucket; "never
   recovered" excludes them and they render as a separate plain line. The
   live-era "both feeds were off today, no contest" / "OFF today (excluded
   from verdict)" frames become REST-era plain wording ("Live price boards
   are retired — brokers are compared by their once-a-minute official
   prices instead."). Classification mechanics unchanged.
5. **Probe-ping reword** — `ChainEntitlementConfirmed` +
   `GrowwChain1mProbeVerdict` gain `cadence_recording: bool` (threaded from
   `[cadence].enabled` via the task params); with cadence ON the passed-arm
   body says recording is handled by the running minute-cadence engine
   instead of the misleading "turn ON the setting and restart".

## Edge Cases

- Exactly 1,500 rows (4 indices × 375 minutes) → `truncated = false`
  (the LIMIT+1 probe); 1,501+ → `true` (honest at any cap size).
- `noise_only` never applies when the run is degraded / truncated /
  blind / partial / no_data, when any minute is missing on either broker,
  or when an open/close field diverged — those all stay High.
- Pre-boot dedup: predecessor rows land at the same minute keys the gap
  walk produces (minute-open nanos) — the exclusion set is exact-key;
  micros→nanos projection mirrors the crossverify query's `(ts/1)*1000`.
- Pre-boot dedup read returning ≥ cap+1 rows (impossible for a 375-minute
  session unless the table is corrupt) → treated as read failure →
  fail-open naming.
- Whole-session blind run (`first_covered = None`) still names every
  session minute not persisted by a predecessor.
- Cadence flag false → both probe pings keep their pre-change wording
  byte-for-byte intent (turn-ON instruction).
- Scorecard days with `-1` sentinel pull records keep sentinel semantics;
  `pre_boot_gaps` uses the same `-1` unavailable sentinel.

## Failure Modes

- QuestDB unreachable during the pre-boot dedup read → coded warn
  (`SPOT1M-01`, stage `pre_boot_dedup_read`) + the OLD behavior (all gaps
  named) — never a silent hole, never a hidden real gap.
- Crossverify query legs degrade exactly as before (degraded/partial/blind
  outcomes unchanged); severity gating only ever DEMOTES the
  pure-noise-diverged case to Info, never a degraded/missing case.
- events.rs render is pure formatting — no new I/O, no new failure path.
- A wrong noise threshold config (negative) is clamped at deserialize
  time by using the serde default only for the absent field; the gating
  treats any diverged field > threshold as High, so a 0 threshold degrades
  to today's always-High behavior (fail-loud direction).

## Test Plan

- `spot_crossverify_boot.rs`: truncation probe pins (`truncated == false`
  at exactly cap rows, `true` at cap+1); severity-gating pins (open/close
  divergence → not noise_only; high/low within threshold → noise_only;
  single delta > threshold → not noise_only; missing minutes → not
  noise_only; degraded → not noise_only); SQL test updated to `LIMIT 1501`.
- `groww_spot_1m_boot.rs`: pre-boot dedup pins (predecessor-persisted
  minutes excluded; read-failure names everything; SQL shape LIMIT+1).
- `events.rs`: render pins (noise_only Info wording + honest frame on
  every arm; rupees formatting; severity fold noise_only → Info);
  scorecard pins updated (pre_boot split line; REST-era verdict wording);
  probe-ping pins (cadence on → "minute-cadence engine — running",
  cadence off → old turn-ON wording).
- Scoped runs: `cargo test -p tickvault-core --lib notification`,
  `cargo test -p tickvault-app --lib spot_crossverify`,
  `cargo test -p tickvault-app --lib groww_spot_1m`,
  `cargo test -p tickvault-app --lib feed_scoreboard`, plus the wiring
  guards. fmt + scoped clippy with no new warnings.

## Rollback

Revert the PR (single squash-merge). No schema change (the audit tables
and DEDUP keys are untouched; `error_class` was already a persisted
column — only the digest SELECT adds it). The config knob is
serde-defaulted, so older TOMLs keep working before AND after revert.
No data migration in either direction.

## Observability

- Counts/counters unchanged (`tv_spot_xverify_*`, `tv_groww_spot1m_*`);
  the SPOT-XVERIFY-01 coded log keeps firing on every divergence with the
  exact counts + noise percentiles (log-sink-only, unchanged).
- New coded warn stage `pre_boot_dedup_read` on SPOT1M-01 (log-only —
  cannot page: the CloudWatch SPOT1M filter is stage="escalation"-scoped).
- The Telegram severity change is the deliverable: pure timing noise
  stops paging High; open/close drift, big deltas, and missing minutes
  still page High (audit Rule 11 — nothing real is silenced).

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — all 15 + 7 rows apply to every item
in this plan. Honest envelope: 100% inside the tested envelope, with
ratcheted regression coverage — the 0-paise exact compare and all audit
counts are unchanged; severity gating is a pure, unit-pinned function;
the pre-boot dedup fails OPEN (a failed read names every gap, never hides
one); no hot-path code is touched (all legs are cold-path post-close
tasks). NOT claimed: that either broker is ground truth (the §37
doctrine — the honest frame now rides every Telegram arm), or that the
₹20 noise threshold is exchange-verified (operator-tunable config with a
documented rationale; the trend counters remain exact either way).

---
**Archived 2026-07-18** — every plan item verified on `origin/main` via PR #1630 (merge commit `2a78010c`, merged 2026-07-17); item 5 (probe-ping reword) landed simplified in the same PR's round-1 review commit (unconditional minute-cadence wording, no `cadence_recording` bool — test-pinned). Archived per coordinator ruling 2026-07-18 (rust-only purge PR 2b-0, V7 plan-cap slot).
