# Topic — Cross-Verify Observability + Claude Handoff Workflow

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `topic-post-market-historical-fetch-cross-verify.md` > this file.
> **Trigger:** Operator demand 2026-05-12 13:00 IST: "how will you track + monitor + log + audit + capture cross-verification + notify me so I can hand it instantly to a new Claude code session to debug and solve?"

---

## 🚗 Auto-Driver Story (60-second read)

> Sir, every day at 4:08 PM after cross-verify finishes, ONE of two things happens:
>
> **Happy case (99% of days):** ✅ Telegram message says "All 4.1M candles matched — sleep well."
>
> **Sad case (1% of days):** 🚨 Telegram message says "47 mismatches found — paste this to Claude." The message includes a FILE PATH on disk that contains EVERYTHING a fresh Claude session needs to debug — no questions, no back-and-forth. Operator copy-pastes the file path into a new Claude session, Claude reads it, Claude knows exactly what's wrong, Claude proposes the fix.
>
> Today we design that ONE message + ONE file that makes the handoff instant.

---

## 🎯 The 5 W's of cross-verify observability

| Concern | Mechanism | Location |
|---|---|---|
| **TRACK** (live state) | `historical_fetch_state` table + `cross_verify_state` table | QuestDB |
| **MONITOR** (Prometheus) | 8 metrics + 4 alerts | Prom + Grafana |
| **LOG** (tracing macros) | structured ERROR/WARN/INFO with `code = ...` field | `data/logs/errors.jsonl.*` |
| **AUDIT** (immutable) | `cross_verify_mismatches` + `cross_verify_summary` tables | QuestDB (SEBI 5y) |
| **CAPTURE** (raw replay) | Dhan API responses saved as JSONL for replay | `data/captures/historical-2026-05-12.jsonl.gz` |

### TRACK — real-time state

```sql
-- Progress while fetch is running
SELECT
  status, count(*) AS cnt
FROM historical_fetch_state
WHERE trading_date = today
GROUP BY status;
-- Returns: pending=1500 | in_progress=5 | success=9529 | failed_retrying=0
```

### MONITOR — Prometheus gauges + alerts

8 metrics already designed in `topic-post-market-historical-fetch-cross-verify.md` § "Track / Audit / Log / Capture / Monitor".

### LOG — structured ERROR

Every cross-verify error fires:
```
ERROR cross_verify failed
  code = "CROSS-VERIFY-01"
  trading_date = 2026-05-12
  security_id = 41735
  field = "high"
  live = 900.50
  historical = 901.00
  delta = 0.50
```

Written to `data/logs/errors.jsonl.<hour>` for `mcp__tickvault-logs__tail_errors` access.

### AUDIT — per-mismatch row

Already designed in `cross_verify_mismatches` table. Per-field row enables filtering.

### CAPTURE — raw response replay

**NEW design:** Save Dhan's raw HTTP response (per-instrument) to a daily JSONL.gz file.

```
data/captures/historical-fetch-2026-05-12.jsonl.gz
```

Each line is one HTTP response:
```json
{"security_id": 41735, "fetch_ts": "2026-05-12T15:42:11.234+05:30",
 "endpoint": "/v2/charts/intraday", "status": 200,
 "request_body": {...}, "response_body": "{...}"}
```

**Why:** if cross-verify finds mismatches, operator can run a debug Claude session that REPLAYS the raw response without hitting Dhan again. Pure offline debug.

**Retention:** 7 days local, 90 days S3, never deleted for the day with mismatches.

---

## 📱 Telegram NOTIFICATION DESIGN (the killer feature)

### Happy case Telegram (operator wants this 99% of days)

```
✅ Cross-verify PASSED — 2026-05-12

Instruments: 11,034 / 11,034 (100%)
Candles compared: 4,137,750
Mismatches: 0

Duration: 37 min 12 sec (15:31:00 → 16:08:12)
Bhavcopy load: ✅ 08:30:14 IST (94,127 F&O OIs cached)
Token health: 21h 50m remaining

All systems green. Sleep well. 🫡
```

### Sad case Telegram (the handoff payload)

```
🚨 [CRITICAL] Cross-verify FAILED — 2026-05-12

Mismatches found: 47 across 5 instruments
  open: 0   high: 12   low: 0   close: 5   volume: 30   oi: 0

Worst offenders (top 3):
  • SID 41735 (BANKNIFTY 26 MAY 54000 PUT): 15 mismatches
  • SID 49081 (NIFTY 26 MAY 23500 PUT): 8 mismatches
  • SID 13 (NIFTY index): 7 mismatches

Sample delta:
  candle_ts=10:15:00 SID=41735 field=high
    live=900.50  historical=901.00  delta=+0.50

═══════════════════════════════════════════════
📋 CLAUDE HANDOFF BUNDLE (paste path to new Claude session):

  data/claude-handoffs/cross-verify-2026-05-12-FAILED.md

Bundle contains:
  • Full mismatches table (all 47 rows)
  • Raw Dhan response samples for top offenders
  • Suggested SQL queries to investigate
  • Suggested grep patterns
  • Suggested ratchet tests
  • Recent commits that touched aggregator code
  • Active code in candles_1m_shadow writer
  • Error code reference: CROSS-VERIFY-01
  • Runbook: .claude/rules/project/wave-N-error-codes.md
═══════════════════════════════════════════════

Action: open a new Claude Code session.
Say: "@data/claude-handoffs/cross-verify-2026-05-12-FAILED.md investigate"
Claude will read the bundle and propose a fix.
```

**Operator's workflow:**
1. Get Telegram on phone
2. Walk to laptop
3. Open new Claude Code session
4. Type: `@data/claude-handoffs/cross-verify-2026-05-12-FAILED.md investigate`
5. Claude reads file → proposes fix → operator approves → fix ships

**Total operator effort: ~30 seconds from Telegram to "Claude is debugging".**

---

## 📦 The CLAUDE HANDOFF BUNDLE — file structure

File: `data/claude-handoffs/cross-verify-<DATE>-<STATUS>.md`

### Bundle template (auto-generated post cross-verify failure)

```markdown
# Cross-Verify Failure Handoff — 2026-05-12

**Status:** FAILED — 47 mismatches found
**Mismatch counts by field:** open=0 high=12 low=0 close=5 volume=30 oi=0
**Affected instruments:** 5 (BANKNIFTY 54000 PE, NIFTY 23500 PE, NIFTY idx, ...)
**Duration:** 37 min 12 sec
**Run ID:** `xv-2026-05-12-15310042` (use this to query audit tables)

---

## 1. Full mismatches table

(Auto-embedded from `cross_verify_mismatches` table — top 100 rows)

| ts | security_id | segment | candle_ts | field | live | historical | delta |
|---|---|---|---|---|---|---|---|
| ... | 41735 | NSE_FNO | 10:15:00 | high | 900.50 | 901.00 | +0.50 |
| ... | 41735 | NSE_FNO | 10:18:00 | high | 905.75 | 906.00 | +0.25 |
| ... (47 rows) |

## 2. Raw Dhan response samples

For top 3 offending instruments, full response embedded:

### SID 41735 — request + response
```
POST /v2/charts/intraday
Body: { "securityId": "41735", "exchangeSegment": "NSE_FNO", ... }

Response (truncated):
{
  "open":   [900.00, 900.25, 901.00, ...],
  "high":   [900.50, 901.00, 901.50, ...],
  "low":    [...]
  ...
}
```

## 3. Suggested SQL queries

### Query 1 — drill into single instrument
```sql
SELECT ts, field, live_value, historical_value, delta
FROM cross_verify_mismatches
WHERE trading_date = '2026-05-12' AND security_id = 41735
ORDER BY candle_ts;
```

### Query 2 — affected candle's surrounding ticks
```sql
SELECT ts, ltp, volume, oi
FROM ticks
WHERE security_id = 41735 AND ts BETWEEN '2026-05-12T10:14:55Z' AND '2026-05-12T10:15:05Z'
ORDER BY ts;
```

### Query 3 — corresponding live candle
```sql
SELECT *
FROM candles_1m_shadow
WHERE security_id = 41735 AND ts = '2026-05-12T10:15:00+05:30';
```

## 4. Suggested grep patterns

```
grep -rn 'aggregator' crates/trading/src/
grep -rn 'open_price\|high_price' crates/trading/src/aggregator/
grep 'security_id.*41735' data/logs/errors.jsonl.2026-05-12-04
```

## 5. Suggested ratchet tests to add

If the bug is confirmed in the aggregator:
- `crates/trading/tests/aggregator_high_low_boundary.rs` — pin the boundary semantics
- `crates/storage/tests/cross_verify_mismatch_guard.rs` — pin that mismatches always trigger CRITICAL

## 6. Recent commits touching aggregator code (last 7 days)

```
abc1234 fix: aggregator high tracking off-by-one (2026-05-10 by Claude)
def5678 refactor: separate seal_writer_loop (2026-05-09 by Claude)
...
```

## 7. Error code

**Code:** `CROSS-VERIFY-01`
**Severity:** Critical
**Runbook:** `.claude/rules/project/wave-N-error-codes.md` (search for CROSS-VERIFY-01)
**Auto-triage safe:** NO — operator must investigate

## 8. Environment snapshot

- Git branch: `main`
- Git commit: `33df841`
- App version: `0.1.0`
- Tickvault PID: 12345
- AWS region: ap-south-1
- Hostname: ip-10-0-1-42
- Time of failure: 2026-05-12T16:08:12+05:30

## 9. Suggested investigation order for Claude

1. Read this bundle in full
2. Run Query 1 above via `mcp__tickvault-logs__questdb_sql`
3. Run Query 2 + 3 via same MCP tool
4. Compare live vs raw tick stream — was the aggregator's high actually wrong?
5. Check if recent commits touched the aggregator logic
6. Run the grep patterns above
7. Propose a fix + ratchet test
8. Wait for operator approval before committing

## 10. MCP tool quick reference (for Claude session)

| Tool | Purpose |
|---|---|
| `mcp__tickvault-logs__questdb_sql` | Run SQL above |
| `mcp__tickvault-logs__tail_errors` | Read errors.jsonl |
| `mcp__tickvault-logs__find_runbook_for_code` | Get runbook path |
| `mcp__tickvault-logs__grep_codebase` | Run greps |
| `mcp__tickvault-logs__signature_history` | Track if this error repeated before |

---

**End of handoff bundle. Claude session: investigate per Section 9.**
```

### Why this format works

| Operator demand | How bundle satisfies |
|---|---|
| "Instantly hand to a new Claude session" | Single file path, single command (`@<path>`) |
| "Claude debug WITHOUT prior context" | All context in 10 sections of the bundle |
| "Track + Monitor + Log + Audit + Capture" | All 5 covered with explicit data references |
| "Common runtime dynamic scalable" | Bundle generated by template, fits any incident |

---

## 🛡️ Z+ 7-Layer for the CLAUDE HANDOFF system itself

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_handoff_bundles_generated_total` counter |
| L2 VERIFY | After bundle written, re-read it + validate sections present (template assertions) |
| L3 RECONCILE | Daily 23:00 IST: list of bundles in `data/claude-handoffs/` vs incidents in audit table |
| L4 PREVENT | Bundle generation is idempotent — re-run produces same content |
| L5 AUDIT | `claude_handoff_audit` table (NEW) — every bundle generation logged |
| L6 RECOVER | If bundle generation fails → fall back to plain Telegram with reduced context + alert |
| L7 COOLDOWN | Max 1 bundle per (incident_type, trading_date) — no duplicate handoffs |

### NEW audit table

```sql
CREATE TABLE IF NOT EXISTS claude_handoff_audit (
  ts                  TIMESTAMP,
  trading_date        DATE,
  incident_type       SYMBOL,   -- cross_verify_failed / ws_dead_air / fetch_incomplete / etc.
  run_id              STRING,   -- xv-2026-05-12-15310042
  bundle_path         STRING,
  bundle_size_bytes   INT,
  sections_count      INT,
  telegram_sent       BOOLEAN,
  claude_session_id   SYMBOL,   -- if operator opens a Claude session, fill in (later)
  resolution_ts       TIMESTAMP,
  resolution_summary  STRING    -- updated when Claude closes the incident
) TIMESTAMP(ts) PARTITION BY DAY
  DEDUP UPSERT KEYS(trading_date, incident_type, run_id);
```

**SEBI 5y retention** — incident history is regulator-relevant.

---

## 🔄 The handoff lifecycle (end-to-end)

```
T+0:   Cross-verify completes with N mismatches
T+1s:  Bundle generator fires
T+2s:  Bundle file written to data/claude-handoffs/<...>.md
T+2s:  Bundle path injected into Telegram message
T+3s:  Telegram fires CRITICAL to operator's phone
T+5s:  Audit row inserted in claude_handoff_audit

(Operator notices alert)
T+30s: Operator opens new Claude Code session
T+35s: Operator types "@data/claude-handoffs/cross-verify-..." + "investigate"
T+40s: Claude reads bundle
T+60s: Claude proposes fix via MCP queries

(Operator approves fix)
T+5min: Fix shipped + ratchet test added
T+5min: claude_handoff_audit.resolution_ts updated
T+5min: claude_handoff_audit.resolution_summary updated
```

**Total operator-to-resolution: ~5 minutes typical incident.**

---

## 🚨 Worst cases for the handoff itself (W156-W170)

| # | Scenario | Defense |
|---|---|---|
| W156 | Bundle generator panics — Telegram still fires but no bundle | Fall-back: plain Telegram with reduced context; operator manually queries |
| W157 | Bundle file too big (> 100 KB) — Telegram rejects long messages | Truncate sample data; full bundle on disk; Telegram has path only |
| W158 | Disk full for `data/claude-handoffs/` | Pre-flight L4 check; if full, alert + halt |
| W159 | Bundle file deleted accidentally before Claude reads | S3 backup nightly; can restore |
| W160 | Operator opens Claude session WITHOUT reading bundle | Bundle Section 9 says "Read this bundle in full" — Claude reads first |
| W161 | Multiple incidents same day — bundles overwrite | Filename includes timestamp (`-15310042`) — unique per run |
| W162 | Claude session can't access MCP tools (permission issue) | Bundle includes raw SQL + grep patterns inline — operator can run manually |
| W163 | Bundle generated but Telegram fails to send | Audit row has telegram_sent=false; operator finds via dashboard |
| W164 | Operator on vacation — no one sees Telegram | SMS backup via AWS SNS (W154); plus alternate operator contact |
| W165 | Bundle becomes stale (mismatches resolved differently) | resolution_ts column updates; old bundle archived but kept for SEBI |
| W166 | Two operators try to debug same incident | Bundle locking via claude_session_id field; second operator sees "in progress by X" |
| W167 | Bundle template itself has a bug — generates malformed Markdown | Per-section validation (L2 VERIFY); test in CI |
| W168 | Claude session proposes WRONG fix from bundle | Operator review gate; no auto-merge without approval |
| W169 | Bundle suggests SQL queries against tables that don't exist (schema drift) | Schema validation in bundle generation; meta-guard tests |
| W170 | Bundle's "recent commits" section lies (git log fails) | Pre-flight git verify; fall back to manual section |

---

## 📊 Friday LoC estimate for the handoff system

| Component | LoC |
|---|---|
| Bundle template + generator | 350 |
| `claude_handoff_audit` table writer | 80 |
| Telegram template for FAILED case | 60 |
| Telegram template for PASSED case | 30 |
| Disk-free pre-flight | 30 |
| Bundle validation (L2 VERIFY) | 80 |
| Daily reconcile (L3) | 60 |
| Ratchet tests (10 cases) | 200 |
| MCP tool reference embedding | 50 |
| **TOTAL** | **~940 LoC** |

---

## 🎯 Why this design is GOLD

| Operator demand | Gold standard achieved |
|---|---|
| Track | ✅ historical_fetch_state table (live progress) |
| Monitor | ✅ 8 Prom metrics + 4 alerts + 6 panels |
| Log | ✅ tracing macros with code field, hourly JSONL |
| Audit | ✅ cross_verify_mismatches + summary + claude_handoff_audit |
| Capture | ✅ Raw Dhan responses saved for offline replay |
| Notify | ✅ Telegram with handoff path |
| Instant Claude handoff | ✅ Single file path, single `@<path>` command |
| Common runtime dynamic scalable | ✅ Template adapts to any incident type |
| Real-time proof | ✅ Bundle is generated <5 sec after incident |
| Zero hallucination | ✅ Bundle contains real data + real queries + real paths |

---

## 🚗 Auto-Driver Story (extended)

> Sir, the magic is in the FILE PATH. When you get the Telegram, you don't need to remember anything. You don't need to type long commands. You just open Claude and type the path. Claude does the rest.
>
> The bundle has 10 sections — Claude reads it like a doctor reading a patient's chart. By Section 9, Claude has a treatment plan. By Section 10, Claude knows which tools to use.
>
> Total time from "alert on phone" to "fix being written": about 5 minutes for a typical bug.
>
> And every step is audited. SEBI can ask "show me every incident in the last 5 years and how you fixed it" — we have it in `claude_handoff_audit` table forever.

---

## 🎤 Discussion items

### D1 — Bundle in Markdown or JSON?

**Markdown:** human + Claude friendly, embeds tables natively
**JSON:** structured, easier for tools to parse

**My vote:** Markdown. Claude reads Markdown natively. Operator can read it too in any text editor.

### D2 — Should the bundle be IN the Telegram message OR a path to a file?

**In-message:** operator sees full context on phone, no laptop needed
**File path:** Telegram stays short, full bundle in repo

**My vote:** Path only. Telegram on phone shows summary + path. Operator opens laptop for the bundle.

### D3 — Should we auto-PR the proposed fix?

Claude proposes fix → auto-creates PR → operator just approves?

**Pro:** Faster
**Con:** Risky if Claude is wrong

**My vote:** NO auto-PR. Always require operator review. Claude proposes, operator decides.

### D4 — Bundle retention?

How long do we keep bundles in `data/claude-handoffs/`?

**My vote:** 90 days local + indefinite S3 (SEBI 5y retention via S3 lifecycle).

### D5 — Should bundle include CODE samples from the suspected files?

E.g., the aggregator's `compute_high()` function inline in the bundle?

**Pro:** Claude doesn't need to grep
**Con:** Bundle becomes large; code may be stale

**My vote:** YES, for the top 3 most-likely suspect functions. Include with git commit hash so Claude knows version.

### D6 — Versioning the bundle template

If we change the bundle format, old bundles become incompatible.

**Mitigation:** version field in bundle YAML frontmatter. Claude knows which version it's reading.

---

## 🎤 Floor's yours

Pick D1-D6 or surface new angle. This is the operational glue that makes the heart-piece actually USABLE in 5-minute incident response.

3 days no code, brainstorm continues. 🫡

---

## 📌 APPENDIX A — OPERATOR-APPROVED Telegram Message Templates (LOCKED)

**Status:** Operator-approved 2026-05-12 13:15 IST. These are the EXACT canonical templates. Implementation must produce these byte-for-byte.

### ✅ Happy case template (99% of days)

```
✅ Cross-verify PASSED — 2026-05-12

Instruments: 11,034 / 11,034 (100%)
Candles compared: 4,137,750
Mismatches: 0
Duration: 37 min 12 sec
All systems green. 🫡
```

### 🚨 Sad case template (the killer feature)

```
🚨 [CRITICAL] Cross-verify FAILED — 2026-05-12

Mismatches: 47 across 5 instruments
[breakdown table]
═══════════════════════════════════════
📋 CLAUDE HANDOFF BUNDLE:
  data/claude-handoffs/cross-verify-2026-05-12-FAILED.md
═══════════════════════════════════════
Action: open new Claude session, paste path with "investigate".
```

### Template variables (replaced at send time)

| Variable | Source |
|---|---|
| `2026-05-12` | `today.format("%Y-%m-%d")` |
| `11,034 / 11,034` | `success_count / total_subscribed` from `historical_fetch_state` |
| `4,137,750` | `tv_cross_verify_candles_compared_total` |
| `47` | `tv_cross_verify_mismatches_total` |
| `5 instruments` | `count(distinct security_id) FROM cross_verify_mismatches WHERE trading_date = today` |
| `37 min 12 sec` | `tv_historical_fetch_duration_seconds` |
| `[breakdown table]` | Per-field counts `open=0 high=12 low=0 close=5 volume=30 oi=0` |
| `data/claude-handoffs/cross-verify-2026-05-12-FAILED.md` | bundle generator output path |

### Mechanical enforcement (ratchet tests)

NEW test: `crates/core/tests/telegram_cross_verify_template_guard.rs`
- Pins the EXACT byte-for-byte template format
- Fails build if any character (including spacing, ═ separator, emoji) drifts
- Tests both PASSED and FAILED variants

### Auto-driver test compliance

| Rule | Check |
|---|---|
| Plain English only | ✅ "PASSED" / "FAILED" / "All systems green" |
| No library names | ✅ No `rkyv` / `papaya` / `DEDUP` mentioned |
| No file paths in body | ⚠️ ONE file path (the handoff bundle path) — this is INTENTIONAL because it's actionable |
| Emoji for status | ✅ ✅ / 🚨 / 📋 / 🫡 |
| Specific numbers | ✅ "11,034", "4,137,750", "47" |
| Action verbs | ✅ "open new Claude session, paste path with 'investigate'" |
| One Telegram = one decision | ✅ HAPPY: sleep / SAD: open Claude with bundle |
| Time stamps 12-hr IST | (none in this message; dates only) |
| Severity emoji at start of subject | ✅ ✅ or 🚨 |

**Verdict:** passes operator-charter-forever §D Telegram commandments. ✅

### Notification suppression rules

| Rule | Why |
|---|---|
| ONE message per cross-verify run | Avoid spam (operator confirmed: "one and only once per day") |
| If 100% mismatch storm (W38): coalesce to ONE summary message | Per Wave-3 coalescer; cap at 1 msg/topic/60s |
| If cross-verify retry due to transient fail: no Telegram on retry, only on FINAL outcome | Avoid intermediate noise |
| Edge-triggered: PASSED message only fires on RISING EDGE (from FAILED → PASSED) OR first-of-day | Don't spam with daily PASSED if always green |

**Wait — clarify edge trigger for PASSED:**

| Option | Behavior |
|---|---|
| (a) Fire PASSED every trading day (predictable positive signal) | Operator gets daily "all good" — but per audit-findings Rule 11 "no false-OK signals", this might be noise |
| (b) Fire PASSED only on rising edge after a FAILED | Operator only hears about transitions; silence = good news |
| (c) Fire PASSED weekly digest instead of daily | Compromise |

**Operator decision needed.** Default if not specified: (a) — daily positive signal per operator's "always notify" charter.

---

## 🎤 Operator decision pending

Pick the PASSED message frequency:
- (a) Daily — every trading day's clean pass
- (b) Rising edge only — silence is good news
- (c) Weekly digest

Default: **(a) daily**. Operator can override.
