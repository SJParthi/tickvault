-- ===========================================================================
-- Groww external-floor latency evidence — paste-ready QuestDB SQL
-- Target: QuestDB 9.3.5 web console (http://<host>:9000) or /exec API.
--
-- VALIDATION STATUS: not yet syntax-validated — QuestDB offline in build env
--   (docker daemon unavailable; `docker compose up` failed on missing SSM
--   credentials; both curl localhost:9000 and the tickvault-logs MCP
--   questdb_sql tool returned "Connection refused" on 2026-07-03).
--   Run each query once on the live box; fix any syntax drift there and
--   update this header to "syntax-validated on QuestDB 9.3.5".
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
--     `exchange_timestamp` column — its units differ per feed (Dhan: IST
--     epoch SECONDS; Groww: also present but not the same basis).
--   * In QuestDB, TIMESTAMP - TIMESTAMP yields LONG **microseconds**;
--     divide by 1000 for milliseconds.
--
-- HONESTY NOTE: until the per-callback capture stamp (capture_ns) lands,
--   `received_at` includes OUR internal share (sidecar + NDJSON + bridge)
--   on top of Groww's server+network share. The per-hour MIN is a hard
--   lower bound on the external floor regardless.
--
-- NULL-CHECK SYNTAX: QuestDB accepts `received_at != null` (its classic
--   null-comparison form); recent versions also accept
--   `received_at IS NOT NULL`. If one form errors on your build, swap to
--   the other — that is the only expected portability issue in this file.
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
       approx_percentile((received_at - ts)/1000, 0.5, 2)  AS p50_ms,
       approx_percentile((received_at - ts)/1000, 0.95, 2) AS p95_ms,
       approx_percentile((received_at - ts)/1000, 0.99, 2) AS p99_ms
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
       approx_percentile((received_at - ts)/1000, 0.5, 2)  AS p50_ms,
       approx_percentile((received_at - ts)/1000, 0.95, 2) AS p95_ms,
       approx_percentile((received_at - ts)/1000, 0.99, 2) AS p99_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) >= 0
  AND ts IN today()
ORDER BY segment;


-- ---------------------------------------------------------------------------
-- Q3: single best-case floor — the 5 lowest deltas observed today, with the
--     instrument + timestamp that achieved each. The global minimum (row 1)
--     fills the "best-case floor observed" line in the dossier.
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
       approx_percentile((received_at - ts)/1000, 0.5, 2)  AS p50_ms,
       approx_percentile((received_at - ts)/1000, 0.95, 2) AS p95_ms,
       approx_percentile((received_at - ts)/1000, 0.99, 2) AS p99_ms
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
       approx_percentile((received_at - ts)/1000, 0.5, 2)  AS p50_ms,
       approx_percentile((received_at - ts)/1000, 0.95, 2) AS p95_ms,
       approx_percentile((received_at - ts)/1000, 0.99, 2) AS p99_ms
FROM ticks
WHERE feed = 'groww' AND received_at != null AND (received_at - ts) >= 0
  AND ts >= '2026-07-03T00:00:00' AND ts < '2026-07-04T00:00:00'
ORDER BY hour_ist, segment;
