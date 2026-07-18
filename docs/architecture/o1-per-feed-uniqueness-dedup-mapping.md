# O(1) Per-Feed Uniqueness · Deduplication · Mapping · Latency — SINGLE SOURCE OF TRUTH

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §F > this file.
> **Status:** PERMANENT reference. Operator directive 2026-06-24: *"I need this
> always … persist … as a committed docs/architecture reference + ratchet … deep
> research … no illusion."*
> **Verified:** 2026-06-24 by 3 parallel read-only agents (uniqueness+dedup ·
> mapping+latency · space+worst-cases). Every claim below is TRUE / PARTIAL /
> FALSE-by-design exactly as the code shows — aspirational text is forbidden.
> **Drift guard:** `crates/storage/tests/o1_per_feed_doc_guard.rs` fails the
> build if this doc loses a core claim OR gains a dishonest one (e.g. "O(1)
> space").

---

## §0. The honest headline (read this first — no illusion)

| Dimension | Honest verdict | Why |
|---|---|---|
| O(1) **time** per tick / lookup / dedup-key / toggle | ✅ **TRUE** | hash get + fixed-offset parse; DHAT + Criterion-budgeted |
| O(1) **per-key overhead** (one fixed row/slot per instrument×feed) | ✅ **TRUE** | composite hash key, SYMBOL-interned columns |
| O(1) **space** *literally* | ❌ **FALSE — never claimed** | storing N instruments × F feeds × 21 TFs is **bounded O(N×F)** — **measured ~4.1 GB** working set on the m8g.large 8 GiB host (`daily-universe-scope-expansion-2026-05-27.md` §7 Rule 2). Bounded ≠ O(1). |
| "100% / never fails / always" | ⚠️ **only with the envelope** | "100% inside the tested envelope, with ratcheted regression coverage" (`operator-charter-forever.md` §F). Beyond it, ring→spill→DLQ catches every payload as recoverable text. |

**The one rule everything hangs off:** every per-feed thing is keyed by the
fixed-width composite **`(security_id, exchange_segment, feed)`** (+ the
designated `ts` / `capture_seq` where the table needs a sub-key). A hash on a
fixed-width key is O(1) time. Add a feed → the key gains one fixed field;
nothing ever turns into a scan.

---

## §1. The four O(1)s — mechanism · verified status · the guard that keeps it true

| O(1) claim | Mechanism (real code) | Verified | Build-failing guard |
|---|---|---|---|
| **O(1) uniqueness** | composite key `(security_id, exchange_segment)` (I-P1-11); `feed` added for per-feed rows. `InstrumentRegistry.by_composite: HashMap<(SecurityId, ExchangeSegment), _>` (`crates/common/src/instrument_registry.rs`) | ✅ TRUE | `dedup_segment_meta_guard::every_dedup_key_with_security_id_must_include_segment` + `per_feed_market_data_dedup_keys_must_include_feed`; banned-pattern scanner cat-5 (no `HashSet<u32>` on instrument paths) |
| **O(1) deduplication** | QuestDB `DEDUP UPSERT KEYS(...)` = **amortized O(1) hash upsert** at the DB; in-RAM a bounded ring prevented loss *(the tick ring + its constant retired 2026-07-18 with the dead tick chain — the live bounded ring is the 200,000-seal `SEAL_BUFFER_CAPACITY` ring)*. **Caveat: dedup is QuestDB-side, NOT in-memory** — duplicates may briefly coexist in the ring and collapse at DB write (correct by design). `capture_seq` is the replay-stable tiebreaker. | ✅ TRUE (DB) / ⚠️ by-design (no in-RAM dedup) | the 3 `dedup_segment_meta_guard` tests + `chaos_index_same_value_burst_preserved.rs` |
| **O(1) mapping** | instrument registry = **std `HashMap`, immutable, `Arc`-shared** (built once at boot, O(1) `get_with_segment`). ISIN→security_id constituent map built ONCE then O(1)/lookup (`constituent_resolver.rs`). **NOTE: the registry is std HashMap, NOT papaya** — papaya is reserved for future dynamic structures; the immutable Arc-shared registry needs no MVCC. | ✅ TRUE | `dhat_instrument_registry.rs` (zero-alloc `get` ×1000) + Criterion `registry_get=50ns` |
| **O(1) latency** | binary parse = fixed-offset `from_le_bytes` (no loop/alloc, `crates/core/src/parser/*.rs`); bounded SPSC (`rtrb`) + bounded broadcast (no unbounded channels); `feed` stamped as `&'static str`/`Copy` enum (`Feed::as_str` const fn) — zero-alloc | ✅ TRUE | `dhat_ws_reader_zero_alloc.rs` (0 allocs/1000 iters) + Criterion budgets `dispatch_frame=10ns`/`pipeline=100ns` + `bench-gate.sh` 5% gate |

---

## §2. Per-feed across the THREE layers (frontend · backend · db)

| Layer | What carries `feed` | How it stays O(1) per feed |
|---|---|---|
| **Frontend** (feed page) | one ON/OFF switch per feed | one `AtomicBool` read — `is_enabled(feed)` is a single lock-free load |
| **Backend** (lanes, pipeline, candles) | `Feed` enum (`Copy`; `as_str()`→`&'static str`; `Feed::index()` for dense array slots) | per-feed state lives in **fixed-size arrays indexed by `Feed::index()`** — no map grows when a feed acts |
| **DB** (QuestDB) | `feed` SYMBOL column **in the DEDUP key** of per-feed tables | QuestDB hashes the key server-side; SYMBOL = interned int, not a string scan |

The 4 feed-keyed market-data DEDUP keys (verified present in code):

| Table | DEDUP key |
|---|---|
| `ticks` | `security_id, segment, capture_seq, feed` (+ designated `ts`) |
| `candles_*` (21) | `ts, security_id, segment, feed` |
| `prev_day_ohlcv` | `ts, security_id, segment, feed` |
| `ws_event_audit` | `ts, trading_date_ist, feed, ws_type, connection_index, event_kind` |

`feed` is a **label only (NOT key)** where the row is shared/combined:
`cross_verify_1m_audit` (Dhan-only), `tick_conservation_audit` (single combined
cross-feed run), and the instrument-master / universe tables (one universe both
feeds watch — keying by feed would duplicate the universe, breaking I-P1-11).

---

## §3. Worst-case / out-of-box coverage → the exact mechanism + test that catches it

| Extreme scenario | What catches it (O(1), correct) | Ratchet test |
|---|---|---|
| Same `security_id` across segments (Dhan reuses ids) | composite key includes `segment` (I-P1-11) | `dedup_regression_nse_bse_1333_collision` + registry `test_iter_returns_both_colliding_segments` |
| Dhan tick **and** Groww tick, same instrument+second | `feed` in the DEDUP key → both kept | `per_feed_market_data_dedup_keys_must_include_feed` |
| Identical value twice (volume-0 index `45→45`) | `capture_seq` tiebreaker → both kept | `chaos_index_same_value_burst_preserved` |
| Replay / reconnect re-sends a frame | same `capture_seq` (read from WAL) → collapses (idempotent) | `dedup_identical_content_replay_idempotent` |
| Add a 3rd feed tomorrow | `Feed::ALL` + exhaustive `match` (no `_` arm) → **compile error** at every site that forgot it | `feed.rs::test_index_is_dense_and_in_lockstep_with_all` |
| QuestDB down / disk full | ring → NDJSON spill → DLQ absorbs (never a blocking scan) | `chaos_ws_frame_spill_saturation`, `chaos_seal_disk_full_dlq_capture` |
| WS disconnect / feed toggled OFF | `SubscribeRxGuard` preserves subs; `wait_until_feed_enabled` parks dormant + reconnects | `chaos_cascade_triple_failure`, `ws_sleep_resilience` |

---

## §4. The HONEST envelope (mandatory wording — `operator-charter-forever.md` §F)

> "100% inside the tested envelope, with ratcheted regression coverage: ≤60s
> QuestDB outage absorbed by rescue→spill→DLQ; ≤200,000-seal ring buffer
> capacity (`SEAL_BUFFER_CAPACITY`, ratcheted by `seal_ring.rs`);
> bench-gated O(1) hot path; composite-key `(security_id, exchange_segment)`
> uniqueness pinned by `dedup_segment_meta_guard.rs`; chaos-tested 65h weekend
> sleep/wake. Beyond the envelope, DLQ NDJSON catches every payload as
> recoverable text."

**Forbidden (auto-reject in review):** "O(1) space", "in-memory O(1) dedup"
(dedup is QuestDB-side), "WebSocket never disconnects", "QuestDB never fails",
"100% coverage" *unqualified*. The honest coverage statement is **ratcheted floors**
(measured 2026-06-10: common ~99.6%, api ~98.7%, trading ~97.0%, storage ~91.4%,
core ~90.4%, app ~63.4%) that **can only move up** —
`quality/crate-coverage-thresholds.toml` + `scripts/coverage-gate.sh`. 100% is
the target, not a current literal.

**Two further honest caveats from the verification:**
- `ws_frame_spill::next_frame_seq()` calls `SystemTime::now()` (a syscall) —
  monotonic + replay-stable, but a syscall, so "O(1)" here means amortized
  constant work with syscall latency (~100ns), not a register op.
- QuestDB DEDUP being O(1)-amortized is standard indexed-UPSERT behaviour, not a
  property we prove in-repo; the in-repo guarantee is the **key shape** (the
  meta-guards) + the **loss-prevention** chain (ring→spill→DLQ).

---

## §5. Common-runtime · dynamic · scalable

| Property | How |
|---|---|
| **Common runtime** | same `Feed` enum + same DEDUP keys Mac-dev = AWS-prod; one code path |
| **Dynamic** | feeds flip ON/OFF live (one atomic) — no redeploy (`wait_until_feed_enabled` + the Groww bridge) |
| **Scalable** | adding instruments/feeds adds fixed-size rows/slots, never converts a get into a scan; bounded channels + ring; space grows **O(N×F) bounded**, measured against the 8 GiB host budget before go-live |

---

## §6. Auto-driver explanation (Insta-reel)

> Sir, every juice slip has 3 stamps — fruit-id, market, supplier. The register
> finds any slip by those 3 stamps in ONE glance (O(1)), never flipping
> page-by-page. Two suppliers selling the same fruit? Two slips, both kept — the
> supplier stamp tells them apart. Add a 3rd supplier? The register's printer
> REFUSES to run until every counter has a slot for him — so no supplier is ever
> silently forgotten. The register's *speed* is constant; its *size* still grows
> with how many fruits × suppliers you stock — we never pretend the book is
> weightless, we just keep it bounded and weigh it (measured ~4.1 GB) before
> opening the shop.

---

## §7. Maintenance (how to keep this doc honest)

- This doc is ratcheted by `crates/storage/tests/o1_per_feed_doc_guard.rs`.
  Changing a feed-keyed DEDUP key, or weakening the honest envelope here, fails
  the build.
- If a future change makes a NEW claim, it must be VERIFIED against code (cite
  file:line) before it lands here — re-run the 3-agent verification panel.
- Cross-referenced from `.claude/rules/project/data-integrity.md` (per-feed
  identity section).
