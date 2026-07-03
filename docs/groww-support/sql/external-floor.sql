-- ===========================================================================
-- Groww external-floor latency evidence — paste-ready QuestDB SQL
-- Target: QuestDB 9.3.5 web console (http://<host>:9000) or /exec API.
--
-- VALIDATION STATUS: docs-validated 2026-07-03 against the official QuestDB
--   documentation source (github.com/questdb/documentation):
--   approx_percentile(value, percentile[, precision 0–5]) over non-negative
--   numeric input; TIMESTAMP − TIMESTAMP = LONG microseconds (native column
--   resolution); timestamp_floor('h', ts) valid; today() returns an interval
--   usable with IN; count() and min(double) valid; both `!= null` and
--   `IS NOT NULL` accepted (≥6.3). NOT yet executed on a live 9.3.5 instance
--   (QuestDB offline in build env) — run once on the live box and update
--   this header to "run-validated".
--
-- PRECISION RULE: never divide by integer 1000 INSIDE the aggregate —
--   integer division truncates to whole ms and floors sub-ms deltas to 0,
--   destroying the ms-floor signal. Percentiles are computed over the raw
--   LONG microsecond deltas; the /1000.0 ms conversion happens OUTSIDE the
--   aggregate.
--
-- SCAN NOTE: `ts IN today()` is valid but may not use the
--   designated-timestamp interval scan; for a guaranteed interval scan use
--   Q6's explicit-bounds pattern.
--
-- MEASUREMENT BASIS (the same-basis rule — do not change):
--   * `ts`          = designated timestamp — the Groww server stamp
--                     (tsInMillis, UTC-ms) converted to IST wall-clock nanos,
--                     full millisecond precision preserved.
--   * `received_at` = our receipt stamp (TIMESTAMP, IST wall-clock),
--                     stamped in the Rust bridge (one clock read per drain
--                     wake). Nullable — older rows may carry NULL.
--   * The ONLY clean latency delta is (received_at - ts): both operands are
--     IST wall-clock TIMESTAMPs. Do NOT subtract the raw
--     `exchange_timestamp` column — it is SECOND-granular on BOTH feeds
--     (Groww's mirrors Dhan's IST-epoch-seconds convention, ms truncated —
--     crates/storage/src/groww_persistence.rs:268-270); `ts` is the only
--     ms-true stamp, so use `ts`.
--   * In QuestDB, TIMESTAMP - TIMESTAMP yields LONG **microseconds**;
--     divide by 1000.0 for milliseconds.
--
-- HONESTY NOTE: until the per-callback capture stamp (capture_ns) lands,
--   `received_at` includes OUR internal share (sidecar + NDJSON + bridge)
--   on top of Groww's server+network share. The per-hour MIN is a hard
--   lower bound on the external floor regardless.
--
-- NULL-CHECK SYNTAX: QuestDB documents both `received_at != null` (its
--   classic null-comparison form) and `received_at IS NOT NULL` (≥6.3) —
--   either form works.
--
-- NON-NEGATIVE GUARD: approx_percentile requires non-negative inputs
--   (HdrHistogram-backed), hence the `(received_at - ts) >= 0` filter in
--   Q1/Q2/Q5/Q6 and the dedicated negative-count sanity query Q4.
--   approx_percentile signature used: approx_percentile(value, percentile,
--   precision) — 3 args. If your build rejects the 3-arg form, drop the
--   trailing precision argument (2-arg fallback) and note which form worked.
-- ===========================================================================


-- ---------------------------------------------------------------------------
-- Q1: per-hour × per-segment latency distribution (today, feed='groww')
--     Fills the "Results — per hour × per segment" table in the dossier.
-- ---------------------------------------------------------------------------
SELECT timestamp_floor('h', ts) AS hour_ist, segment,
       count() AS ticks,
       min((received_at - ts)/1000.0) AS min_ms,
       approx_percentile(received_at - ts, 0.5, 2)/1000.0  AS p50_ms,
       approx_percentile(received_at - ts, 0.95, 2)/1000.0 AS p95_ms,
       approx_percentile(received_at - ts, 0.99, 2)/1000.0 AS p99_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) >= 0
  AND ts IN today()
ORDER BY hour_ist, segment;


-- ---------------------------------------------------------------------------
-- Q2: per-segment all-day distribution (today, feed='groww')
--     Fills the "Results — per segment, all day" table in the dossier.
-- ---------------------------------------------------------------------------
SELECT segment,
       count() AS ticks,
       min((received_at - ts)/1000.0) AS min_ms,
       approx_percentile(received_at - ts, 0.5, 2)/1000.0  AS p50_ms,
       approx_percentile(received_at - ts, 0.95, 2)/1000.0 AS p95_ms,
       approx_percentile(received_at - ts, 0.99, 2)/1000.0 AS p99_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) >= 0
  AND ts IN today()
ORDER BY segment;


-- ---------------------------------------------------------------------------
-- Q3: single best-case floor — the 5 lowest deltas observed today, with the
--     instrument + timestamp that achieved each. The global minimum (row 1)
--     fills the "best-case floor observed" line in the dossier.
--
--     NOTE: the `ticks` table has NO symbol / exchange_token columns. Before
--     pasting into the dossier, resolve each security_id to its Groww trading
--     symbol + exchange_token per the README instrument-label convention,
--     e.g.:
--       SELECT symbol_name, display_name FROM instrument_lifecycle
--       WHERE security_id = <id> AND feed = 'groww' LIMIT 1;
-- ---------------------------------------------------------------------------
SELECT security_id, segment, ts,
       (received_at - ts)/1000.0 AS delta_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) >= 0
  AND ts IN today()
ORDER BY delta_ms ASC
LIMIT 5;


-- ---------------------------------------------------------------------------
-- Q4: negative-delta sanity count (clock-skew guard).
--     Rows where received_at < ts mean the receipt stamp precedes the server
--     stamp — impossible on a sane clock, so any non-trivial count signals
--     host clock skew (cross-check BOOT-03) or a stamping bug. These rows
--     are EXCLUDED from Q1/Q2/Q5/Q6 (approx_percentile requires >= 0), so
--     this query makes the exclusion visible instead of silent.
-- ---------------------------------------------------------------------------
SELECT segment,
       count() AS negative_delta_rows,
       min((received_at - ts)/1000.0) AS worst_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) < 0
  AND ts IN today()
ORDER BY segment;


-- ---------------------------------------------------------------------------
-- Q5: same as Q1 but feed='dhan', for a side-by-side comparison.
--     CAUTION when interpreting: Dhan's `ts` is second-granular (exchange
--     seconds × 1e9), so the sub-second component of every Dhan delta is
--     quantized to the second boundary — Dhan deltas carry up to ~1000 ms
--     of quantization noise and are NOT directly comparable to Groww's
--     millisecond-true deltas. Use for order-of-magnitude context only.
-- ---------------------------------------------------------------------------
SELECT timestamp_floor('h', ts) AS hour_ist, segment,
       count() AS ticks,
       min((received_at - ts)/1000.0) AS min_ms,
       approx_percentile(received_at - ts, 0.5, 2)/1000.0  AS p50_ms,
       approx_percentile(received_at - ts, 0.95, 2)/1000.0 AS p95_ms,
       approx_percentile(received_at - ts, 0.99, 2)/1000.0 AS p99_ms
FROM ticks
WHERE feed = 'dhan' AND received_at != null AND (received_at - ts) >= 0
  AND ts IN today()
ORDER BY hour_ist, segment;


-- ---------------------------------------------------------------------------
-- Q6: date-parameterized variant of Q1 — for a day other than today.
--     Swap BOTH date literals to the target day (start inclusive, end
--     exclusive): e.g. for 2026-07-01 use
--       ts >= '2026-07-01T00:00:00' AND ts < '2026-07-02T00:00:00'
-- ---------------------------------------------------------------------------
SELECT timestamp_floor('h', ts) AS hour_ist, segment,
       count() AS ticks,
       min((received_at - ts)/1000.0) AS min_ms,
       approx_percentile(received_at - ts, 0.5, 2)/1000.0  AS p50_ms,
       approx_percentile(received_at - ts, 0.95, 2)/1000.0 AS p95_ms,
       approx_percentile(received_at - ts, 0.99, 2)/1000.0 AS p99_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) >= 0
  AND ts >= '2026-07-03T00:00:00' AND ts < '2026-07-04T00:00:00'
ORDER BY hour_ist, segment;
