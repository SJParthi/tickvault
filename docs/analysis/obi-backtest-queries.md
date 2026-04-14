# OBI Backtest & Analysis Queries

QuestDB SQL queries for analyzing Order Book Imbalance (OBI) data from the `obi_snapshots` table.

## Table Schema

```sql
obi_snapshots (
    segment SYMBOL,
    security_id LONG,
    underlying SYMBOL,
    obi DOUBLE,           -- [-1, +1] simple imbalance
    weighted_obi DOUBLE,  -- [-1, +1] distance-weighted
    total_bid_qty LONG,
    total_ask_qty LONG,
    bid_levels LONG,
    ask_levels LONG,
    max_bid_wall_price DOUBLE,
    max_bid_wall_qty LONG,
    max_ask_wall_price DOUBLE,
    max_ask_wall_qty LONG,
    spread DOUBLE,
    received_at TIMESTAMP,
    ts TIMESTAMP           -- designated timestamp, partitioned by HOUR
)
```

## 1. OBI Distribution by Underlying

How often is OBI positive (buying pressure) vs negative (selling pressure)?

```sql
SELECT
    underlying,
    count() AS total_snapshots,
    count() FILTER (WHERE obi > 0.3) AS strong_buy,
    count() FILTER (WHERE obi BETWEEN -0.3 AND 0.3) AS neutral,
    count() FILTER (WHERE obi < -0.3) AS strong_sell,
    round(avg(obi), 4) AS avg_obi,
    round(min(obi), 4) AS min_obi,
    round(max(obi), 4) AS max_obi
FROM obi_snapshots
WHERE ts > dateadd('d', -7, now())
GROUP BY underlying
ORDER BY total_snapshots DESC;
```

## 2. OBI vs Price Movement Correlation

Compare OBI direction with subsequent price movement (requires joining with tick data).

```sql
-- Step 1: Sample OBI at 1-minute intervals
WITH obi_1m AS (
    SELECT
        ts,
        underlying,
        security_id,
        avg(obi) AS avg_obi,
        avg(weighted_obi) AS avg_wobi
    FROM obi_snapshots
    WHERE ts > dateadd('d', -1, now())
    SAMPLE BY 1m
    FILL(PREV)
)
-- Step 2: Join with tick LTP at matching intervals
-- (Run separately — QuestDB CTEs limited)
SELECT
    o.ts,
    o.underlying,
    o.avg_obi,
    t.last_traded_price AS ltp
FROM obi_1m o
ASOF JOIN ticks t ON (o.security_id = t.security_id)
WHERE o.ts > dateadd('d', -1, now())
ORDER BY o.ts;
```

## 3. Wall Detection Frequency

How often do walls appear, and on which side?

```sql
SELECT
    underlying,
    count() FILTER (WHERE max_bid_wall_qty > 0) AS bid_walls,
    count() FILTER (WHERE max_ask_wall_qty > 0) AS ask_walls,
    round(avg(max_bid_wall_qty) FILTER (WHERE max_bid_wall_qty > 0), 0) AS avg_bid_wall_size,
    round(avg(max_ask_wall_qty) FILTER (WHERE max_ask_wall_qty > 0), 0) AS avg_ask_wall_size,
    count() AS total
FROM obi_snapshots
WHERE ts > dateadd('d', -7, now())
GROUP BY underlying
ORDER BY bid_walls + ask_walls DESC;
```

## 4. Wall Absorption Rate

When a wall appears, how quickly does it get absorbed (disappear)?

```sql
-- Bid walls appearing then disappearing within the hour
SELECT
    ts,
    underlying,
    max_bid_wall_price,
    max_bid_wall_qty,
    lead(max_bid_wall_qty, 60) OVER (PARTITION BY underlying ORDER BY ts) AS qty_60s_later
FROM obi_snapshots
WHERE ts > dateadd('h', -1, now())
    AND max_bid_wall_qty > 0
ORDER BY ts;
```

## 5. Spread Analysis

Average spread by time of day — wider spreads near open/close?

```sql
SELECT
    underlying,
    hour(ts) AS hour_ist,
    round(avg(spread), 4) AS avg_spread,
    round(min(spread), 4) AS min_spread,
    round(max(spread), 4) AS max_spread,
    count() AS samples
FROM obi_snapshots
WHERE ts > dateadd('d', -7, now())
GROUP BY underlying, hour(ts)
ORDER BY underlying, hour_ist;
```

## 6. OBI Momentum (Rolling Average)

5-minute rolling OBI to smooth out noise.

```sql
SELECT
    ts,
    underlying,
    obi,
    avg(obi) OVER (PARTITION BY underlying ORDER BY ts ROWS BETWEEN 300 PRECEDING AND CURRENT ROW) AS obi_5m_avg,
    avg(weighted_obi) OVER (PARTITION BY underlying ORDER BY ts ROWS BETWEEN 300 PRECEDING AND CURRENT ROW) AS wobi_5m_avg
FROM obi_snapshots
WHERE ts > dateadd('h', -1, now())
ORDER BY ts;
```

## 7. OBI Extremes (Signal Detection)

Timestamps where OBI hit extreme values — potential reversal signals.

```sql
SELECT
    ts,
    underlying,
    obi,
    weighted_obi,
    total_bid_qty,
    total_ask_qty,
    spread
FROM obi_snapshots
WHERE ts > dateadd('d', -1, now())
    AND (obi > 0.7 OR obi < -0.7)
ORDER BY abs(obi) DESC
LIMIT 100;
```

## 8. Bid/Ask Quantity Ratio Over Time

Track how the total quantity ratio evolves through the trading day.

```sql
SELECT
    ts,
    underlying,
    total_bid_qty,
    total_ask_qty,
    CASE WHEN total_ask_qty > 0
        THEN round(cast(total_bid_qty AS DOUBLE) / cast(total_ask_qty AS DOUBLE), 2)
        ELSE 0
    END AS bid_ask_ratio
FROM obi_snapshots
WHERE ts > dateadd('h', -1, now())
    AND underlying = 'NIFTY'
SAMPLE BY 10s
FILL(PREV)
ORDER BY ts;
```

## 9. Data Quality Check

Verify OBI data completeness.

```sql
SELECT
    underlying,
    count() AS total_rows,
    min(ts) AS first_ts,
    max(ts) AS last_ts,
    count() FILTER (WHERE bid_levels = 0 AND ask_levels = 0) AS empty_snapshots,
    round(avg(bid_levels), 1) AS avg_bid_levels,
    round(avg(ask_levels), 1) AS avg_ask_levels
FROM obi_snapshots
WHERE ts > dateadd('d', -1, now())
GROUP BY underlying
ORDER BY underlying;
```
