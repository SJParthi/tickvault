# Archived sections — daily-universe-scope-expansion-2026-05-27.md

> Sections excised verbatim from `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
> on 2026-07-20 (context-size incident). Live file keeps a pointer per section.


<!-- ==== §1 The rule (retired subscription contract) ==== -->

## §1. The rule — one paragraph

This product, starting from the date of this lock, opens exactly TWO WebSocket connections to Dhan FOREVER (unchanged from prior lock): one main-feed + one order-update. The main-feed connection subscribes to a **daily universe** of approximately **250 instrument SecurityIds** — **all NSE indices in IDX_I segment + 1 BSE SENSEX in IDX_I segment + the unique F&O underlying spot instruments (NSE_EQ)** — pulled fresh every trading morning at 08:45 IST from Dhan's static Detailed CSV. Every subscription is in **Quote mode** (Feed Request Code 17, 50-byte response packets carrying day OHLC). The previous 4-SID `LOCKED_UNIVERSE` const + `SubscriptionScope::Indices4Only` single-variant enum + the 4-IDX_I-only ratchet are RETIRED. The new compile-time enum variant is `SubscriptionScope::DailyUniverse`. The host instance upgrades from t4g.medium 4 GiB → **t4g.large 8 GiB** to hold today + yesterday sealed bars across all 21 timeframes for ~250 SIDs in RAM.

---



<!-- ==== §3 Dhan Detailed CSV source + §4 infinite retry policy (retired 2026-07-13) ==== -->

## §3. Source of the daily universe — Dhan Detailed CSV (LOCKED)

| Field | Locked value |
|---|---|
| URL | `https://images.dhan.co/api-data/api-scrip-master-detailed.csv` |
| Authentication | NONE (public static file) |
| HTTP method | GET |
| Expected size | 5–15 MB |
| Trigger time | 08:45 IST daily (after EC2 cold-boot + Step 1-6 auth completes) |
| Refresh policy | Daily — yesterday's local copy overwritten |
| Verification | SHA-256 + row count + per-row schema validation |
| Reject threshold | If >0.1% of rows fail mandatory-field validation → reject + retry. Mandatory fields: `SECURITY_ID`, `EXCH_ID`, `SEGMENT`, `INSTRUMENT`, `UNDERLYING_SECURITY_ID` (for derivatives), `SYMBOL_NAME` |
| Cross-validation | Every `UNDERLYING_SECURITY_ID` cited by FUTSTK/OPTSTK rows MUST exist as an NSE_EQ row in the same CSV — dangling references reject the CSV |

**No fallback to a different data source.** Per operator Quote 2: "no need of any api pull in case of any failures". Specifically forbidden:
- Per-segment REST `/v2/instrument/{exchangeSegment}` fallback — banned
- REST `/v2/marketfeed/ltp` as a price source — banned
- S3 cached snapshot of yesterday's CSV — banned
- Continuing boot with stale `instrument_lifecycle` data — banned

---

## §4. Infinite retry policy (LOCKED per operator Quote 4)

| Attempt | Backoff before next | Telegram severity | Action |
|---|---|---|---|
| 1–3 | 10s / 20s / 40s | None | Silent retry |
| 4–10 | min(2^N × 10s, 300s) | `Severity::Info` every attempt | Counter `tv_instrument_fetch_retries_total` increments |
| 11–20 | capped 300s | `Severity::High` every 5 attempts | Operator notified |
| 21+ | capped 300s | `Severity::Critical` every 5 attempts | Operator paged on every channel |
| ∞ | Never give up | — | Boot remains BLOCKED. No subscription dispatch. No fallback. |

**Boot stays BLOCKED until a fresh, validated CSV is in hand and the daily universe is built.** The system NEVER silently proceeds with stale or partial data. At 09:00 IST with no fresh CSV: market opens, no ticks flow, operator receives Critical and decides whether to manually intervene (e.g. check Dhan status, fix DNS, fix egress). The honest outcome is documented — never camouflaged.

**No-give-up rationale:** the operator's literal demand is "never ever fail … without the proper fetch it should retry". Fail-closed means we refuse to proceed with the wrong universe; the retry never terminates because there is no acceptable alternate path.

---



<!-- ==== §8 Quote-mode subscription, §9 Z+ fetch defense, §10 boot sequence, §11 mechanical guards, §12 REJECT list, §13 honest claim, §14 auto-driver (retired subscription chain) ==== -->

## §8. Subscription mode — Quote for every SID (LOCKED per operator Quote 2)

Every SID in the daily universe — indices, F&O underlyings, **and the §36/§36.7 FUTIDX contracts (2026-07-08; all monthly expiries since 2026-07-10)** — subscribes in **Quote mode**:

> §36 note (2026-07-08): in Quote mode, derivative OI is NOT inline (inline OI bytes 34-37 exist only in the Full packet) — OI arrives as the separate 12-byte code-5 packet (live-market-feed.md rule 9), which the tick processor currently counts-and-drops.

- Feed Request Code: `17` (Subscribe — Quote Packet)
- Response Code: `4` (Quote Packet)
- Packet size: **50 bytes**
- Carries: LTP, LTQ, LTT (IST epoch secs), ATP, Volume, Total Sell Qty, Total Buy Qty, **day_open** (bytes 34-37), **previous-day close** (bytes 38-41), day_high (bytes 42-45), day_low (bytes 46-49)

**Why Quote and not Ticker:** the Quote packet delivers session day OHLC at fixed byte offsets, removing the need for app-side `DayOhlcTracker` state for the universe-wide subscription. For derivatives prev-close routing, Quote mode is correct for NSE_EQ underlyings per `live-market-feed.md` rule 10. (Indices in `IDX_I` segment also receive Quote-mode packets per operator's 2026-05-20 directive — `DayOhlcTracker` falls back to first-tick LTP auto-arm if Dhan ignores Quote for IDX_I.)

**Bandwidth envelope:** 250 SIDs × 50 B/packet × ~20 ticks/sec sustained ≈ 250 KB/sec. Within AWS egress + Dhan WebSocket limits.

---

## §9. Z+ 7-layer defense for the instrument-master fetch

Per `.claude/rules/project/z-plus-defense-doctrine.md`:

| Layer | Mechanism | What it catches |
|---|---|---|
| **L1 DETECT** | HTTPS GET on `images.dhan.co/api-data/api-scrip-master-detailed.csv`; check 200 OK + content-length > 1 MB | Network failure, Dhan 5xx, empty response |
| **L2 VERIFY** | Parse CSV header row; SHA-256 of payload; row count; mandatory-column presence; TLS cert validation | Schema drift, partial download, corruption, DNS poisoning |
| **L3 RECONCILE** | Compare row count + SHA-256 vs yesterday's `instrument_fetch_audit`; flag if `total_rows < 0.5× yesterday OR > 2× yesterday` | Silent Dhan-side bulk changes, stale-CSV serving |
| **L4 PREVENT** | Validate parsed universe size in `[100, 1200]`; HALT if outside. Verify every `UNDERLYING_SECURITY_ID` from FUTSTK/OPTSTK rows resolves to an NSE_EQ row in the same CSV (no dangling references). | Misparsed CSV; runaway subscription cost |
| **L5 AUDIT** | `instrument_fetch_audit` row per attempt (success or failure) + `instrument_lifecycle_audit` row per state transition + `tv_universe_size{kind=...}` CloudWatch metric + tracing span | Continuous observability + forensic chain |
| **L6 RECOVER** | Infinite retry (per §4). NEVER fallback to a different data source. NEVER proceed with stale data. | Operator-locked: no silent degradation |
| **L7 COOLDOWN** | Retry backoff capped at 300s between attempts; never burst Dhan with >5 GETs/min | Self-induced rate limit |

---

## §10. Boot sequence integration

The new `instrument_lifecycle` orchestrator slots between existing Step 6 (auth) and the WebSocket pool spawn:

> *(2026-07-19 note: the timeline below is HISTORICAL — the §10 orchestrator retired with the Dhan subscription contract per the 2026-07-13 top banner, and the "EC2 r8g.large boots" line predates the 2026-07-15 Quote 8 downsize; the host has been **t4g.medium** since then — §7 + PR #1582 / downsize-instance.yml are the current authority.)*

```
08:30 IST  EventBridge cron fires (Mon–Fri only); EC2 r8g.large boots (~60s cold)
08:31      Docker compose up (QuestDB + tickvault-app)
08:31:30   App Step 1-5: CryptoProvider → Config → Observability → Logging → Notification
08:32      Step 6: Dhan auth (TOTP → JWT) — Valkey dual-instance lock acquired
08:32:30   Step 6a: Dhan static IP boot gate (GET /v2/ip/getIP)
08:33      Step 6b: QuestDB DDL — includes new `instrument_lifecycle` + `instrument_lifecycle_audit` tables
08:33:30   ─── NEW: Step 6c — Daily universe orchestrator ─────────────────────
              1. Read yesterday's active set from `instrument_lifecycle` (cold-path bootstrap)
              2. GET Dhan Detailed CSV with L1-L7 defense layers (§9)
              3. Validate + parse + SHA-256
              4. Extract (a) indices (filter §2) + unique F&O underlyings (group by UNDERLYING_SECURITY_ID) → the 331-SID SUBSCRIPTION set (+ the §36.7 all-months FUTIDX rows, 2026-07-10); AND (b) the applicable F&O CONTRACTS (FUTSTK/OPTSTK for resolved underlyings + FUTIDX/OPTIDX for tracked indices; currency/commodity excluded) → MASTER-only set per §5
              5. Compute delta vs yesterday — emit added / expired / renamed transitions (over the full master set incl. contracts)
              6. UPSERT `instrument_lifecycle` (subscription set + applicable-F&O contracts per §5) + INSERT `instrument_lifecycle_audit`
              7. INSERT `instrument_fetch_audit` outcome row
              8. Build `Arc<DailyUniverse>` for the WS subscription dispatcher — dispatcher reads `subscription_targets` ONLY (331), NEVER the contracts (2-WS lock) — the §36.7 all-months FUTIDX rows are promoted INTO `subscription_targets` at build time (2026-07-10), so the dispatcher contract itself is unchanged
              9. Bust rkyv binary cache; rebuild for tomorrow's warm boot
              [Infinite retry on any L1-L4 failure per §4]
08:34      Step 7: Spawn 1 main-feed WebSocket; subscribe Quote mode in 3 batches
08:34:30   Step 8: Spawn order-update WebSocket
08:35      Phase-1 boot complete; listening for ticks (10 min before market open)
08:45      ── soft deadline — if not subscribed by here, fire Severity::High Telegram
08:55      ── hard deadline — if not subscribed by here, fire Severity::Critical Telegram
09:00      Market opens — universe is live
```

**Boot-time-of-day guard:** if `now > 08:55 IST` at orchestrator start, refuse to begin a fresh-fetch cycle without explicit operator override flag (`--allow-late-boot`). Avoids mid-market universe rebuild attempts.

**Day 1 bootstrap:** if `instrument_lifecycle` table is empty (very first run), every CSV row gets `first_seen_date = today`, `lifecycle_state = active`, NO delta-expiry pass. Detected by SELECT COUNT(*) before the orchestrator's flip pass.

**`--dry-run-universe` CLI flag (per option Z approval):** fetches + validates + computes universe + writes audit row but does NOT subscribe. Use on first production boot to verify pipeline end-to-end without committing to live ticks.

---

## §11. Mechanical guards (to be wired by Sub-PR #1 onward)

| Guard | What it enforces | Test file (Sub-PR that adds it) |
|---|---|---|
| `SubscriptionScope::DailyUniverse` enum variant | Compile-time scope contract; retires `Indices4Only` | `crates/common/src/config.rs` (Sub-PR #1) |
| `effective_main_feed_pool_size(_, _) → 1` | Main feed always 1 conn (UNCHANGED) | `crates/common/src/config.rs` (Sub-PR #1) |
| `MAX_DAILY_UNIVERSE_SIZE = 1200` constant | Universe size cap; HALT if exceeded | `crates/common/src/constants.rs` (Sub-PR #2/#7) |
| `crates/storage/tests/instance_type_lock_guard.rs` | Source-scan: pins `t4g.large` in `aws-budget.md` + this file + architecture doc §5 | NEW in Sub-PR #1 |
| `crates/storage/tests/daily_universe_scope_guard.rs` | Source-scan: pins Detailed CSV URL + DEDUP key contract + Quote-mode-for-all + retry-policy unbounded loop + I-P1-11 composite key | NEW in Sub-PR #1 |
| Source-scan retirement of `indices4only_scope_lock_guard.rs` | Old ratchet removed | RETIRED in Sub-PR #1 |
| Per-row CSV schema validation | Reject CSV if >0.1% rows fail mandatory-field check | `crates/core/src/instrument/csv_parser.rs` (Sub-PR #4) |
| Lifecycle reconciler test coverage | Idempotent UPSERT + state-flip + dangling-reference rejection | `crates/storage/tests/instrument_lifecycle_*.rs` (Sub-PR #9) |
| RAM-first hot path (UNCHANGED) | banned-pattern scanner blocks SELECT in indicator/strategy/risk paths | already shipped |
| `INDEX_FUTURES_UNDERLYINGS` const (len 4) + `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING = 6` + never-roll all-months selector | §36/§36.7 FUTIDX scope pinned in code + rule text | `crates/storage/tests/daily_universe_scope_guard.rs::{futidx_scope_pinned_to_4_underlyings_all_monthly_expiries, futidx_scope_rule_file_pins_forbidden_remainder, futidx_scope_never_roll_source_pin, futidx_scope_legacy_gate_still_false}` (§36 2026-07-08; §36.7 2026-07-10) |

---

## §12. What a PR that violates this lock looks like (REJECT)

- Removes the `DailyUniverse` enum variant or adds a parallel variant without a dated operator quote
- Re-introduces `Indices4Only` or `FullUniverse` or `IndicesOnlyAllExpiries` or `IndicesUnderlyingsOnly` variants
- Adds a fallback data source (REST LTP, S3 cache, yesterday's stale CSV) to the instrument-fetch path
- Adds a give-up condition to the fetch retry loop (any code path returning Err without retry)
- Changes the subscription mode for ANY universe SID from Quote to Ticker or Full without a dated operator quote
- Adds a 2nd main-feed connection or any new WS endpoint
- Subscribes the daily universe to derivative contracts **beyond the §36 grant** (OPTIDX/FUTSTK/OPTSTK always; FUTIDX beyond the 4 named underlyings, beyond monthly serials `>= today`, or beyond the `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING` envelope) — otherwise only the UNDERLYING_SECURITY_ID spot rows are subscribed (§36 2026-07-08; §36.7 2026-07-10)
- DELETES rows from `instrument_lifecycle` (lifecycle_state transitions are the ONLY allowed mutation; no DELETE statements)
- Changes `effective_main_feed_pool_size` to anything other than 1
- Modifies instance type from t4g.medium without the 4-file update protocol in §7 Mechanical Rule 1 (t4g.medium per Quote 8, 2026-07-15 — this row previously guarded r8g.large)

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land. This prevents accidental scope creep through casual approvals.

---

## §13. Honest 100% claim (mandatory wording per `operator-charter-forever.md` §F)

When any PR / commit message / Telegram body invokes "100% guarantee" for the daily-universe pipeline, it MUST be qualified exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> infinite retry on instrument CSV fetch with escalating Telegram (Info→High→Critical at attempts 4/11/21);
> app boot BLOCKS until fresh CSV in hand — never proceeds with stale, partial, or corrupt data;
> seal bursts absorbed via the 200,000-seal ring (constant `SEAL_BUFFER_CAPACITY`, ratcheted by `seal_ring.rs`) → NDJSON spill → DLQ catches every payload as recoverable text
> (the tick rescue ring + its constant were retired 2026-07-18 with the dead tick chain — stage-4 sweep);
> all 21 timeframes RAM-resident for today + yesterday across the current universe (~770 SIDs, Groww-only runtime; app cap ~1.5 GB per §7 Rule 2, ~0.9–1.7 GB budgeted headroom on the t4g.medium 4 GiB host — headroom Assumed until live-measured; wording updated 2026-07-15 per Quote 8, was "~3.2 GB working set, ~7.8 GB headroom on the r8g.large 16 GiB host");
> `instrument_lifecycle` table is SEBI-compliant — NO DELETEs ever, only state transitions to `expired_*` SYMBOLs preserved in `(security_id, exchange_segment)` composite-key history per I-P1-11;
> daily lifecycle reconciler captures every appearance, disappearance, rename, segment-move, split — logged to `instrument_lifecycle_audit` forensic chain with 5-year SEBI retention."

Stronger phrasing ("never miss a tick", "WebSocket never disconnects", "QuestDB never fails") without the envelope qualifier = REJECT IN REVIEW.

---

## §14. Auto-driver / Insta-reel explanation

> Sir, imagine the juice shop boy goes to the fruit market every morning at 8:30 AM — BEFORE the shop opens at 9 AM. He picks up the FRESH list of every fruit available today. He throws away yesterday's list. From today's list he picks: (a) every type of basket index — NIFTY, BANKNIFTY, SENSEX, FINNIFTY, all the sectoral baskets, even the one BSE SENSEX basket. (b) For every fruit that has a futures or options contract (apple-Dec-100-Call, mango-Jan-200-Put, etc.), the boy notes the underlying fruit's spot price slot. He doesn't subscribe to every contract — just the underlying spot — about 250 spots total.
>
> If the boy can't get the list (network down, Dhan server down), he keeps trying FOREVER — never gives up, never substitutes a stale list, never makes one up. The phone rings (Telegram) at 5 minutes, then 15 minutes, then every 5 minutes after that — but the shop refuses to open without the fresh list. Better closed than open with the wrong list.
>
> Old fruits that disappear from today's list (e.g., a stock that left the F&O list) stay in the register marked "expired" — never deleted. The register is the SEBI auditor's friend: every appearance, disappearance, rename, split is logged with a timestamp. Five years later we can still answer "was Mazagon Dock in the universe on 27 May 2026?"

---



<!-- ==== Sub-PR #1.5 enrichment preamble (historical) ==== -->

# Sub-PR #1.5 enrichment (2026-05-27) — adversarial 3-agent findings

The 3-agent adversarial review run during Sub-PR #1 (hot-path-reviewer +
security-reviewer + general-purpose hostile bug-hunt) produced 8 CRITICAL,
17 HIGH, 12 MEDIUM and 5 LOW findings. The CRITICAL/HIGH items are
captured below as locked contracts for the subsequent sub-PRs to
implement. Sub-PR #1.5 ships these contract additions as pure markdown
(no Rust code) so the contracts exist BEFORE the implementation PRs land.

---



<!-- ==== §20 operator escape valve, §21 sub-PR ordering gate, §22 holiday handling, §23 split/rename classification, §24 audit-chain ordering (retired fetch chain / shipped history) ==== -->

## §20. Operator escape valve — `--operator-acknowledge-stale-csv <sha256>` (Sub-PR #2)

Addresses hostile finding **O-C3** (SHA-256 reject loop deadlock).

The §4 infinite-retry policy means a permanently corrupt Dhan CSV (e.g. their CDN cache poisoning, Dhan-side bug serving the same broken file) blocks boot FOREVER with no escape. This is correct safety semantics but leaves operator without a manual override.

**The ONE override path** (no other escape valve, ever):

```
tickvault --operator-acknowledge-stale-csv <expected_yesterday_sha256> [other flags...]
```

Semantics:
1. Operator MUST paste yesterday's SHA-256 explicitly. Can NOT be silently triggered.
2. App reads yesterday's `instrument_lifecycle` table snapshot.
3. App verifies `expected_yesterday_sha256` matches what's in yesterday's `instrument_fetch_audit.csv_sha256`. If mismatch → reject the flag, continue infinite retry.
4. If match → app proceeds with yesterday's lifecycle as ground truth FOR ONE TRADING DAY ONLY.
5. Write a `instrument_fetch_audit` row with `outcome = stale_csv_operator_override`, `operator_note = <required free text>`.
6. Telegram Severity::Critical: "OPERATOR OVERRIDE — running with yesterday's CSV. Investigate Dhan CSV corruption."
7. App boot continues; subscription dispatched against yesterday's universe.

**On the NEXT day's boot:** the flag is NOT remembered. Operator must either fix the upstream issue OR re-pass the flag with FRESH SHA-256.

**Ratchets (Sub-PR #2):**

- `crates/app/tests/operator_override_csv_guard.rs::test_operator_override_requires_yesterday_sha256_match`
- `crates/app/tests/operator_override_csv_guard.rs::test_operator_override_writes_critical_telegram`
- `crates/app/tests/operator_override_csv_guard.rs::test_operator_override_does_not_persist_to_next_day`

---

## §21. Sub-PR ordering safety — compile-time feature gate

Addresses hostile finding **O-C2** (PR ordering safety).

If Sub-PR #7 (default flip to `DailyUniverse`) merges BEFORE Sub-PR #3 (CSV downloader) + #4 (parser) + #9 (lifecycle reconciler), the app would boot with `DailyUniverse` scope but no fetcher implementation — INSTR-FETCH-01 Critical forever, unrecoverable.

**Mechanical guard (added in Sub-PR #1.5 as a contract; code lands in Sub-PR #3+):**

```toml
# crates/common/Cargo.toml
[features]
default = []
daily_universe_fetcher = []  # toggled on by Sub-PR #9 ONLY
```

The `DailyUniverse` variant currently shipped in Sub-PR #1 (PR #810) is gated by `#[cfg(feature = "daily_universe_fetcher")]` in Sub-PR #3 onward, so production code can't switch to it until the fetcher exists.

**Sequencing locked:**
- Sub-PRs #1, #1.5, #2, #6 (rule-file + boot-orchestrator scaffolding) — no feature flag toggle
- Sub-PRs #3, #4 (downloader + parser) — code lives behind the feature flag
- Sub-PR #9 (lifecycle reconciler) — flips `daily_universe_fetcher = ["..."]` on
- Sub-PR #7 (default flip) — now safe to land
- Sub-PR #10 (boot orchestrator wiring) — references the variant unconditionally

**Ratchets (Sub-PR #3):**

- `crates/common/src/config.rs::tests::test_daily_universe_variant_only_compiles_with_feature_flag`
- `crates/storage/tests/daily_universe_feature_flag_guard.rs::test_feature_flag_not_default_enabled`

---

## §22. Holiday / Muhurat / declared-holiday handling (Sub-PR #2 must implement)

Addresses hostile findings **O-H1, O-H4** (holiday CSV thrash + Muhurat trading).

The daily orchestrator MUST consult `TradingCalendar::is_holiday(today_ist)` BEFORE entering the §4 infinite-retry loop:

| Day classification | Orchestrator behaviour |
|---|---|
| Regular trading day | Full §4 algorithm — fetch, validate, build universe, subscribe |
| Saturday / Sunday | Single fetch attempt with 30s timeout. If success: noop log (SHA-256 likely matches yesterday). If failure: log Severity::Info, exit clean. No subscription dispatch (market closed). |
| Declared holiday (Republic Day / Independence Day / Diwali Laxmi Pujan etc.) | Same as Sat/Sun |
| Muhurat trading day (Diwali) | Special evening start — EC2 cron OVERRIDES from 08:30 morning to 17:30 IST (1 hour before Muhurat session). Operator must pre-populate the calendar OR pass `--muhurat-mode 18:00` flag. |
| End-of-fiscal-year (Mar 31) | Normal day — Dhan publishes normal CSV; nothing special. |

**Why this matters:** infinite retry against a holiday's stale Dhan CSV would hammer their CDN at 12 GETs/hour × 24 hours = ~280 GETs and risks WAF rate-limit ban.

**Ratchets (Sub-PR #2):**

- `crates/core/tests/holiday_orchestrator_guard.rs::test_holiday_orchestrator_skips_retry_loop_on_declared_holiday`
- `crates/core/tests/holiday_orchestrator_guard.rs::test_holiday_orchestrator_runs_normal_on_trading_day`
- `crates/core/tests/holiday_orchestrator_guard.rs::test_muhurat_mode_flag_overrides_cron`

---

## §23. Stock-split / symbol-rename / segment-move classification (Sub-PR #9)

Addresses hostile findings **O-H2** (stock split) + **O-H3** (rename chain).

The §6 `transition_kind` SYMBOL enum is extended:

| New value | Trigger | Audit row payload |
|---|---|---|
| `split` | `lot_size_new < lot_size_old × 0.5` OR `tick_size_new < tick_size_old × 0.5` | `field_deltas = {"lot_size": [old, new], "tick_size": [old, new]}`, Telegram Severity::High "Stock split: TCS old_lot=1000 new_lot=200" |
| `segment_moved` | Same `security_id` appears under a different `exchange_segment` than yesterday | Severity::High Telegram. New row in lifecycle (composite key includes segment, so it's a DISTINCT row), old row marked `expired_*`. |

`prev_symbol` upgraded from SYMBOL → STRING (JSON array, append-only):

```sql
ALTER TABLE instrument_lifecycle DROP COLUMN prev_symbol;
ALTER TABLE instrument_lifecycle ADD COLUMN prev_symbol_chain STRING;
```

Example chain: `["HDFC", "HDFCBANK_TMP"]` after two renames.

**Ratchets (Sub-PR #9):**

- `crates/core/tests/lifecycle_split_detection_guard.rs::test_lot_size_half_or_less_classified_as_split`
- `crates/core/tests/lifecycle_split_detection_guard.rs::test_split_writes_high_severity_telegram`
- `crates/core/tests/lifecycle_rename_chain_guard.rs::test_three_hop_rename_preserves_full_chain`
- `crates/core/tests/lifecycle_segment_move_guard.rs::test_segment_move_creates_new_row_and_expires_old`

---

## §24. Audit-chain ordering — write-audit-first (Sub-PR #9)

Addresses hostile finding **O-H7** (partial audit-chain write on QuestDB ILP error).

If the orchestrator UPSERTs `instrument_lifecycle` and then crashes BEFORE INSERTing `instrument_lifecycle_audit`, the forensic chain is broken.

**Locked write order (Sub-PR #9):**

```
For each lifecycle transition:
  1. INSERT INTO instrument_lifecycle_audit (transition_kind = "pending", ...)
  2. UPSERT INTO instrument_lifecycle (...)
  3. UPDATE instrument_lifecycle_audit SET transition_kind = "<actual_kind>" WHERE id = <step1_id>
```

If step 2 or 3 fails, we have a `pending` audit row recording the attempt — better than no audit at all.

**Ratchet (Sub-PR #9):**

- `crates/storage/tests/lifecycle_audit_ordering_guard.rs::test_audit_row_written_before_lifecycle_upsert`

---



<!-- ==== §26 CSV parser robustness + §27 dry-run isolation (retired fetch chain) ==== -->

## §26. CSV parser robustness — BOM / line endings / quoted commas (Sub-PR #4)

Addresses hostile finding **O-H8** + security-reviewer **S-L2** (BiDi unicode).

The CSV parser MUST handle:

| Edge case | Behaviour |
|---|---|
| BOM (`\xEF\xBB\xBF`) at file start | Strip silently |
| CRLF / LF / CR mixed line endings | Normalize to LF |
| Quoted field with embedded comma `"REL,IND"` | Parse correctly via `csv` crate `Trim::All` mode |
| UTF-8 strict | Reject ISO-8859-1 bytes outside 7-bit ASCII |
| BiDi override characters in `symbol_name` | Strip via `sanitize_audit_string` (already in `crates/common/src/sanitize.rs`) |
| Row with >0.1% mandatory-field failures | Reject whole CSV per §3 |
| Single-row corruption ≤0.1% | Drop the row, log Severity::High, continue |
| Dangling `UNDERLYING_SECURITY_ID` reference (no NSE_EQ row matches) | Threshold > 0.5% derivative rows → reject CSV; below → drop the affected derivative rows + log Severity::High |

**Ratchets (Sub-PR #4):**

- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_strips_utf8_bom`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_normalizes_mixed_line_endings`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_handles_quoted_commas`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_rejects_iso_8859_1`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_strips_bidi_overrides_in_symbol_name`
- proptest: `crates/core/tests/csv_parser_property.rs::arbitrary_mutations_either_parse_or_reject_cleanly`

---

## §27. Dry-run universe isolation (Sub-PR #10)

Addresses hostile finding **O-H9**.

The `--dry-run-universe` flag (per option Z approval) writes `instrument_lifecycle` + `instrument_fetch_audit` + `instrument_lifecycle_audit` rows. Without isolation, a Day-1 dry-run leaks into Day-2 live run as ground truth.

**Schema additions (Sub-PR #9 / #10):**

Both `instrument_lifecycle` and `instrument_lifecycle_audit` gain:

- `dry_run` BOOLEAN

Daily orchestrator reads ONLY `WHERE dry_run = false` rows for delta computation. Lifecycle reconciler writes the column value matching the orchestrator's runtime mode.

**Ratchet (Sub-PR #10):**

- `crates/core/tests/dry_run_isolation_guard.rs::test_dry_run_rows_not_used_for_next_day_delta`

---



<!-- ==== §29 warm-resubscribe snapshot + §31 NTM subscription authorization (retired subscription chain; §31.1 mapping contract kept live) ==== -->

## §29. Same-day warm-resubscribe snapshot — operator authorization 2026-05-29

**Operator quote (2026-05-29, this session):**
> "yes go ahead bro merge all the PRs let em run this atalst bro okay?"
> — given in direct response to the disaster-recovery analysis showing
> that an OOM / crash / Docker restart / QuestDB-volume wipe at 11:30 IST
> should NOT force the full cold rebuild before ticks flow again.

**What this authorizes (and bounds):** a date-keyed subscription-plan
snapshot written to the host disk (`data/instrument-cache/plan-snapshot-YYYY-MM-DD.json`),
SEPARATE from the QuestDB volume, used for INSTANT warm-resubscribe on a
**same-day** boot. This is a narrowly-scoped relaxation of §4's "boot
BLOCKS until a fresh validated CSV is in hand" rule — and ONLY for the
same-day-crash case.

| Boot case | Behaviour | §4 fail-closed still applies? |
|---|---|---|
| FIRST boot of the trading day (no snapshot) | Full cold build, infinite retry, boot BLOCKS until fresh CSV | YES — unchanged |
| SAME-DAY re-boot (snapshot present, date matches today) | Instant plan from snapshot (~1ms) → ticks flow → background reconcile refreshes lifecycle master + snapshot | Relaxed for first-tick; reconcile runs in background |
| Snapshot from a PREVIOUS day | Rejected (date mismatch) → falls through to cold build | YES |
| Snapshot corrupt / unknown role | Rejected (fail-closed) → falls through to cold build | YES |

**Why this is NOT "proceeding with stale data" (the §4 prohibition):** a
same-date snapshot means today's universe was already fetched, validated,
and lifecycle-written by an earlier successful boot. The snapshot is a
cache of THAT computation, not a substitute for a missing fetch. The
background reconcile re-runs the full §4 cold build (infinite retry,
fail-closed) and refreshes both the lifecycle master and the snapshot —
so the audit/lifecycle work is DEFERRED past first-tick, never SKIPPED.

**Honest envelope:** this does NOT detect an intraday universe change; it
is a same-day warm cache keyed on the IST trading date. Correctness of
"is today's universe still valid" remains the background reconcile's job.
If the background reconcile fails, ticks still flow from the warm plan and
the next boot retries — lifecycle for TODAY was already written by the
boot that produced the snapshot.

**Source + ratchets:**
- `crates/core/src/instrument/instrument_snapshot.rs` (module + 12 unit tests)
- `crates/app/src/main.rs::load_daily_universe_plan` (fast-path + slow-path),
  `cold_build_daily_universe` (shared helper), `record_instrument_load_telemetry`
- Path-traversal guard: `instrument_snapshot::is_valid_trading_date` (strict
  `YYYY-MM-DD`, fail-closed) — test `test_is_valid_trading_date_rejects_traversal_and_malformed`
- Fail-closed role parse — test `test_to_universe_fails_closed_on_unknown_role`
- Background-reconcile failure logs `error!` with `code = INSTR-FETCH-01`,
  does NOT halt the live warm plan.

---

# §31 — NIFTY Total Market + index→stocks subscription (operator authorization 2026-06-06)

**Operator decisions (2026-06-06, captured via AskUserQuestion — authoritative):**
1. Add a **33rd index — NIFTY Total Market** — to the tracked set (today 31 NSE
   allowlist + 1 BSE SENSEX = 32).
2. Build an **index → its constituent stocks** mapping for every tracked index.
3. **Subscribe ALL NIFTY Total Market constituent stocks (~750)** to the live feed
   in **Quote mode**, on the **existing single main-feed WebSocket** (NO new WS).
4. The other 32 indices: **map only**. The live SUBSCRIPTION set is the **NTM
   union** (NTM is the broadest basket and already contains their members).
5. **F&O stocks remain separately extractable** — each subscribed stock carries a
   **role tag** (`fno_underlying` and/or `index_constituent`, with the owning
   index list). "F&O only" is an **O(1) filter** over the same data — NOT a second
   download and NOT a second subscription/WebSocket.
6. **`MAX_DAILY_UNIVERSE_SIZE` raised 400 → 1200** to fit ~1,000 live SIDs +
   headroom. **DONE in Sub-PR #2 (2026-06-06):** the §2/§9-L4 envelope is now
   `[100, 1200]`; the seal-ring headroom guard floor moved 20× → 5× (still
   ~7.9× at the 1200 cap, well above the chaos-tested 2× IST-midnight-burst
   adequacy; the 200K `SEAL_BUFFER_CAPACITY` L-C1 design lock is preserved).
7. Constituency source: **niftyindices.com** (rebuild the deleted downloader; reuse
   the existing `INDEX_CONSTITUENCY_*` constants). **Operator-provided exact URL
   (2026-06-06):** the NIFTY Total Market constituents (~750 stocks) live at
   `https://www.niftyindices.com/IndexConstituent/ind_niftytotalmarket_list.csv`
   (i.e. `INDEX_CONSTITUENCY_BASE_URL` + `ind_niftytotalmarket_list.csv`). This is
   the constituent-STOCK list for Sub-PR #3/#4; the NIFTY Total Market INDEX value
   itself (the IDX_I allowlist entry) needs its exact Dhan master `SYMBOL_NAME`,
   resolved from the live Detailed master in Sub-PR #3 (NOT guessed).

**What is UNCHANGED (still locked):**
- Exactly **2 WebSocket connections** (1 main-feed + 1 order-update). ~1,000 SIDs
  fit on the one main-feed conn (Dhan cap 5,000/conn). The 2-WS lock holds.
- **Quote mode** for every SID (§8).
- **`(security_id, exchange_segment)` composite uniqueness** + DEDUP UPSERT KEYS
  (I-P1-11). The NTM-union subscription is deduped against the F&O underlyings.
- `instrument_lifecycle` is still NEVER deleted; the new `role` is an added column,
  applied via `ALTER TABLE ADD COLUMN IF NOT EXISTS`.
- Indicators/strategies boundary (§28) is untouched by this work.

**Honest envelope (mandatory per §13 / operator-charter §F):**
- RAM at ~1,000 SIDs × 21 TFs ≈ ~3.2 GB working set on the r8g.large 16 GiB host
  (~7.8 GB headroom). This is a **MEASURED gate** in the persistence/validation
  sub-PR — NOT promised blind. If the measurement exceeds budget, the scope or the
  resident-TF set is revisited with the operator before go-live.
  **2026-07-15 Quote 8 supersession note:** the host is now **t4g.medium 4 GiB**
  (§7) — a ~1,000-SID ~3.2 GB working set does NOT fit next to QuestDB@1g + OS on
  4 GiB. Today's Groww-only ~770-SID runtime is budgeted per §7 Rule 2 with
  ~0.9–1.7 GB budgeted headroom (Assumed until live-measured). Any NTM-scale re-expansion
  must re-pass the measured RAM gate on the 4 GiB host — or re-size the instance
  per §7 Mechanical Rule 1 — before go-live.
- "~750 NTM stocks" is the expected count; the EXACT count comes from the live
  niftyindices.com constituent file at build time (no hard-coded hallucinated number).

**Implementation sequencing** lives in `.claude/plans/active-plan.md` (Sub-PR #1 =
this authorization record; Sub-PRs #2–#7 = cap+allowlist, downloader, mapping +
role tagging, subscription wiring, persistence + observability + RAM proof, e2e
validation). Each is a separate serial PR with the 15+7 guarantee matrix and the
adversarial 3-agent review. No code sub-PR may widen the subscription beyond the
NTM union without re-confirming against this section.

---



<!-- ==== §0 Quotes 1–7 (2026-05-27..2026-06-30 — universe expansion, m8g/r8g instance history, all superseded by Quote 8's t4g.medium downsize + the 2026-07-13 subscription retirement) ==== -->

**Quote 1 (2026-05-27, universe expansion):**
> "now in our logic once again we need to pull the entire instruments as it is dude … before 9 am around 8.45 am itself the app should be started and then it should pull the instruments entirely every new instruments file entirely and it should find the entire unique fno instruments and along with that we need all nse indexes also separately"

**Quote 2 (2026-05-27, BSE SENSEX inclusion + Quote mode + no API fallback + AWS start time):**
> "all nse indices along with one sensex bse index also needed dude entirely for all of them it should be quote mode dude always okay? no need of any api pull in case of any failures okay? see as of now AWS should be started only at 8.45 am if everything can be handled and tackled precisely then go ahead and approved"

**Quote 3 (2026-05-27, instance upgrade + tick loss + RAM + 21 TFs + CloudWatch-only):**
> "ok dude go ahead with t4g large dude previously i believe we already launched t4g medium dude that one i believe then now our script auto script should update it to large right dude? see irrespective of any situations not even a single tick should be missed meanwhile everything should be in memory RAM right bro even along with calculating entire 21 timeframes i believe bro only cloudwatch alone as well bro okay?"

**Quote 4 (2026-05-27, infinite retry + lifecycle preservation):**
> "see irrespective of any situations it should never ever fail i mean until or unless without the proper fetch it should retry right that too covering all kinds of entire extreme worst case scenarios errors exceptions situations bugs … for future or options it should be just marked as expired and active alone only right dude instead of deleting it right dude"

**Quote 5 (2026-05-29, instance = m8g.large + weekday-only schedule):**
> "why m8 why not c8 or r8 dude?" … "see only on tarding working days our plan is to run only 8.30 am till 4.30 pm max dude … then after that whenever manually i need to run the isnatcne i will do it"
>
> Operator authorized: (a) instance lock → **m8g.large** (Graviton4, latest gen, 2 vCPU / 8 GiB, general-purpose — m-family chosen over c8g [4 GiB, too little RAM] and r8g [16 GiB, wasteful] because 8 GiB needs the 4:1 general-purpose ratio); live ap-south-1 on-demand = **$0.06416/hr**. (b) Schedule → **trading weekdays only (Mon–Fri), 08:30–16:30 IST auto start/stop**; operator starts manually for any out-of-window run. This is the dated quote §7 Mechanical Rule 1 + §12 require before the instance + schedule change.

**Quote 6 (2026-05-29, EBS + no EIP + <₹2,000 target for data-pull):**
> "for storage atleast we need 100 gb right so even in later phase we can easily move everything to s3 right dude" … "why ebs is so costly why not internal storage dude?" … "it should be less than 2000 dude thats our plan" … "exclude static ip dude since for the next three months no real orders … lets say 270 hours"
>
> Operator authorized + resolved: (c) EBS gp3 **30 GB** (not 100 — 100 GB pushed the all-in bill to ~₹2,698 incl GST, over the operator's <₹2,000 target; 30 GB hot window + S3 archival of >90d partitions keeps it ~₹2,058. Internal/instance NVMe rejected — wiped on daily stop). gp3 grows online; raise anytime. (d) **Elastic IP excluded** (`enable_eip = false`) for the 3-month no-orders data-pull — saves ~₹430/mo; re-enable before live trading. (e) cost basis = **270 running hrs/month**. (f) Tax = **18% GST** total (IGST inter-state, or CGST 9% + SGST 9% intra-state — same 18%, no extra cess). All-in ≈ **₹2,058/mo incl. GST** (~₹58 over the ₹2,000 target — operator accepted; drop to 20 GB for strictly-under).

**Quote 7 (2026-06-30, instance = r8g.large 16 GiB — supersedes the 2026-05-29 m8g.large lock):**
> "just upgrade the instance to r8 large + everything related to the infra"
>
> Operator authorized: the host instance upgrades from **m8g.large** (2 vCPU /
> 8 GiB, general-purpose) → **r8g.large** (Graviton4, 2 vCPU / **16 GiB**,
> memory-optimized r-family) — DOUBLING RAM for the upcoming both-feeds +
> larger-universe workload. Live ap-south-1 on-demand = **$0.08258/hr**. The
> **Elastic IP is KEPT** (`enable_eip = true`, the 2026-05-31 flip — without it
> the box has NO public IP after a stop/modify/start, so it could reach neither
> SSM nor Dhan; see Terraform `enable_eip` description). Everything related to
> the infra moves to r8g.large in lockstep per §7 Mechanical Rule 1: this rule
> file §7, the superseded `aws-budget.md` + `aws-indices-only-locked-architecture.md`
> §5 markers, the `instance_type_lock_guard.rs` ratchet, the Terraform validation,
> the upgrade-script `FROM_TYPE` default (now `m8g.large`), and the Docker
> QuestDB `QDB_MEM_LIMIT` default (2g → 4g, comfortable in 16 GiB). This dated
> quote satisfies §7 Mechanical Rule 1 for the instance change.
> **Post-resize note (2026-07-01):** the resize is now DONE (box is physically
> `r8g.large`/16 GiB, effective at the 08:30 IST auto-start), so the docker-compose
> `mem_limit: ${QDB_MEM_LIMIT:-4g}` default is now 4g directly — no longer coupled
> to `scripts/aws-upgrade-instance.sh` (which #1274/#1278 had used to keep the
> compose default at 2g until the physical resize). The 2K-universe
> expansion (`MAX_DAILY_UNIVERSE_SIZE` / `SEAL_BUFFER_CAPACITY`) is a SEPARATE
> later PR gated on a live memory measurement and is NOT changed here;
> `dry_run` stays `true`.



<!-- ==== §0 Approvals 2026-05-27..2026-06-30 (superseded history) ==== -->

**Approvals:**
- 2026-05-27: Approved Sub-PR plan items A–D (infinite retry policy, single `instrument_lifecycle` table, separate `instrument_lifecycle_audit` table, plan growing to 14 sub-PRs)
- 2026-05-27: Approved options X–Z (EventBridge cron at 08:30 IST, `lifecycle_state_locked` column for operator overrides, `--dry-run-universe` CLI flag for first prod validation)
- 2026-05-29: Approved instance m8g.large (Graviton4, 8 GiB) + weekday-only 08:30–16:30 IST auto schedule (manual start otherwise) per Quote 5
- 2026-05-29: Approved EBS 100 GB + EIP excluded (no orders) + 270-hr basis → ~₹2,698/mo incl GST per Quote 6
- 2026-06-02: Operator WIDENED the schedule from 08:30–16:30 IST to **08:00–17:00 IST** (verbatim: "instead of 8.30 am make it as 8 am till 5 pm dude so that pre-market and post-market and deployment and all other activities can run without any worries"). Crons: start `cron(30 2 ? * MON-FRI *)` (02:30 UTC = 08:00 IST), stop `cron(30 11 ? * MON-FRI *)` (11:30 UTC = 17:00 IST). Cost: +1 hr/day (~+₹120/mo), still inside the ~₹2,058/mo envelope. This dated quote satisfies §7 Mechanical Rule 1 for the schedule change.
- 2026-06-05: Operator NARROWED the schedule back from 08:00–17:00 IST to **08:30–16:30 IST** (verbatim: "make the aws instance start and stop from 8.30 am till 4.30 pm dude one and only when it is needed let me start it manually"). Crons: start `cron(0 3 ? * MON-FRI *)` (03:00 UTC = 08:30 IST), stop `cron(0 11 ? * MON-FRI *)` (11:00 UTC = 16:30 IST). The start-watchdog ping/check move to 08:30/08:45 IST; the GitHub-Actions after-close start cron + `aws-autopilot.sh`/`deploy-aws.yml` up-window move to 08:30–16:30 in lockstep. Cost: −1 hr/day (~−₹120/mo). The 08:30 start still gives the documented §10 boot budget before the 09:00 pre-open. This dated quote satisfies §7 Mechanical Rule 1 + §12 for the schedule change and supersedes the 2026-06-02 widening.
- 2026-06-30: Approved instance UPGRADE m8g.large → **r8g.large** (Graviton4, 2 vCPU / 16 GiB) + EIP KEPT per Quote 7; bill ~₹2,058/mo → ~₹2,919/mo incl GST (270 hrs, 30 GB EBS, +EIP). The 2K-universe expansion is deferred to a separate later PR.
- 2026-07-13: Approved EBS grow 30 GB -> 50 GB gp3 (+~₹170/mo incl GST) + instance-role S3 write on tv-prod-cold/groww-capture/* (Groww capture archival), per operator quote: "go ahead and merge everything once it is green - yes do whatever is the recommendation" (prod disk-pressure remediation - disk hit 82% on 2026-07-13 with zero reclamation). Note: the instance role's existing cold-bucket statement (main.tf, s3:GetObject/PutObject/ListBucket on the whole `tv-<env>-cold` bucket) ALREADY covers the groww-capture/* prefix — no IAM change was needed. Bill ~₹2,919/mo → ~₹3,101/mo incl GST (recomputed below).
