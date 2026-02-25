# IntelliJ IDEA Ultimate — Database Connections

> **Prerequisite:** Docker stack must be running.
> ```bash
> docker compose -f deploy/docker/docker-compose.yml up -d
> ```

---

## QuestDB (Time-Series Database)

IntelliJ IDEA Ultimate has built-in **Database Tools** that connect to QuestDB via its PostgreSQL wire protocol. This lets you query tick data, orders, and audit trails directly from the IDE — no browser needed.

### Connection Details

| Setting | Value |
|---|---|
| **Datasource Type** | PostgreSQL |
| **Host** | `localhost` |
| **Port** | `8812` |
| **Database** | `qdb` |
| **Username** | `admin` |
| **Password** | `quest` |
| **JDBC URL** | `jdbc:postgresql://localhost:8812/qdb` |

> **Note:** These are QuestDB's default dev credentials, not secrets. Production QuestDB runs behind Docker network with no external port exposure.

### How to Connect

The datasource is pre-configured in `.idea/dataSources.xml` (created by Claude Code, gitignored).

**If you need to recreate it (fresh clone):**

1. Open IntelliJ IDEA Ultimate
2. Open the **Database** tool window (View > Tool Windows > Database, or `Cmd+Shift+D` on Mac)
3. Click **+** > **Data Source** > **PostgreSQL**
4. Enter the connection details from the table above
5. Click **Test Connection** (download the PostgreSQL driver if prompted)
6. Click **OK**

### What You Can Do

Once connected, the **Database** tool window shows:

- **Tables** — Browse all QuestDB tables (tick data, orders, candles, audit logs)
- **Console** — Write and execute SQL queries with autocomplete
- **Data Editor** — View table data in a spreadsheet-like grid with filtering and sorting
- **Diagrams** — Visualize table relationships

### Sample Queries

After connecting, open a new console (`Cmd+Shift+Q` or right-click datasource > New > Console) and run:

```sql
-- List all tables
SHOW TABLES;

-- Check tick data (once Block 01+ is built and collecting)
SELECT * FROM ticks
WHERE timestamp > dateadd('h', -1, now())
ORDER BY timestamp DESC
LIMIT 100;

-- Check order audit trail
SELECT * FROM orders
ORDER BY created_at DESC
LIMIT 50;

-- 1-minute OHLCV candles for a specific instrument
SELECT
    timestamp,
    first(price) AS open,
    max(price) AS high,
    min(price) AS low,
    last(price) AS close,
    sum(volume) AS volume
FROM ticks
WHERE security_id = 12345
SAMPLE BY 1m
ALIGN TO CALENDAR WITH OFFSET '05:30';
```

> **Full query reference:** `scripts/questdb-verify-connection.sql`

---

## Valkey (Cache)

Valkey (Redis-compatible) can be accessed from IntelliJ via the **Redis** plugin (pre-installed in Ultimate).

| Setting | Value |
|---|---|
| **Host** | `localhost` |
| **Port** | `6379` |
| **Database** | `0` |
| **Password** | *(none for dev)* |

**To connect:** Database tool window > **+** > **Data Source** > **Redis** > Enter host/port > **Test Connection** > **OK**

---

## Web Alternatives

All data is also accessible via browser dashboards:

| Service | URL | Purpose |
|---|---|---|
| QuestDB Console | [localhost:9000](http://localhost:9000) | SQL editor + table browser |
| Grafana | [localhost:3000](http://localhost:3000) | Dashboards + alerting (admin/admin) |
| Jaeger | [localhost:16686](http://localhost:16686) | Distributed traces |
| Prometheus | [localhost:9090](http://localhost:9090) | Raw metrics + PromQL |
| Traefik | [localhost:8080](http://localhost:8080) | Reverse proxy dashboard |

---

## Troubleshooting

| Problem | Solution |
|---|---|
| Connection refused | Docker stack not running. Run `docker compose -f deploy/docker/docker-compose.yml up -d` |
| Driver not found | IntelliJ will prompt to download PostgreSQL JDBC driver — click **Download** |
| Authentication failed | Use `admin` / `quest` (QuestDB defaults) |
| No tables visible | Tables are created when the application starts writing data (Block 01+) |
| Slow queries | QuestDB is optimized for time-series. Always filter by `timestamp` range |
