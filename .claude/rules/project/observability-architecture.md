# Zero-Touch Observability Architecture — SINGLE SOURCE OF TRUTH

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every `error!`, `warn!`, `info!`, every file/hook/metric/alert
> in the tickvault observability chain. Future Claude sessions MUST read
> this file before touching logging, alerting, monitoring, or error-handling.
> **Directive:** Parthiban, 2026-04-18 — *"in the future we should never
> ever recreate the same scenario or go through the same condition ...
> nowhere repeating the same process again and again from scratch ...
> I need 100 percent guarantee"*.

## The one-line architecture

```
error! → tracing → [5 sinks] → Telegram + AWS SNS
                                ↓
                         Claude auto-triage
                                ↓
                     known→auto-fix | novel→escalate
```

## The 5 sinks every `error!` hits (in order)

| # | Sink | Where | Purpose | Retention |
|---|------|-------|---------|-----------|
| 1 | stdout / journald | host | `docker logs`, `journalctl -u tickvault` | systemd default |
| 2 | `data/logs/app.YYYY-MM-DD.log` | disk | full app log, daily rotation | `LOG_MAX_FILES` |
| 3 | `data/logs/errors.log` | disk | WARN+ only, single file, grep-friendly | single file |
| 4 | `data/logs/errors.jsonl.YYYY-MM-DD-HH` | disk | **ERROR-only, JSONL, hourly rotation** | 48h (auto-swept) |
| 5 | Telegram + AWS SNS SMS | network | operator / Claude triage | Alertmanager dedup |

Sink 4 is the one future Claude Code sessions and any log-ingestion MCP tail.
`cat data/logs/errors.jsonl.$(date -u +%Y-%m-%d-%H) | jq` = one pipe, every
structured ERROR event in the last hour.

## The ErrorCode taxonomy (53 variants, 100% rule-synced)

Every tracked error/invariant lives in `crates/common/src/error_code.rs`:

- **I-P0-01..06** — Instrument priority-0 (data-loss / correctness)
- **I-P1-01/02/03/05/06/08/11** — Instrument priority-1
- **I-P2-02** — Instrument priority-2 (trading-day guard)
- **GAP-NET-01** — IP monitor ; **GAP-SEC-01** — API auth
- **OMS-GAP-01..06** — Order Management System
- **WS-GAP-01..03** — WebSocket
- **RISK-GAP-01..03** — Risk engine
- **AUTH-GAP-01..02** — Authentication
- **STORAGE-GAP-01..02** — Storage layer
- **DH-901..910** — Dhan Trading API error codes
- **DATA-800/804..814** — Dhan Data API error codes (rules reference
  these as bare backticked numbers — the cross-ref test handles both)

Every variant carries:
- `code_str()` — the stable wire-format string ("I-P1-11", "DH-904", ...)
- `severity()` — `Info < Low < Medium < High < Critical`
- `runbook_path()` — `.claude/rules/*.md` file documenting triage
- `is_auto_triage_safe()` — never true for Critical

## The 21 mechanical ratchets (every one blocks a regression)

| # | Test file | What it guarantees |
|---|-----------|--------------------|
| 1 | `crates/common/src/error_code.rs::test_all_variants_have_unique_code_str` | No code-string collisions |
| 2 | same::`test_code_str_roundtrip_via_from_str` | `ErrorCode -> str -> ErrorCode` identity |
| 3 | same::`test_from_str_rejects_unknown_code` | Unknown input returns typed error, never panics |
| 4 | same::`test_every_variant_has_non_empty_runbook_path` | Every variant points at `.claude/` |
| 5 | same::`test_severity_ordering` | Info < Low < Medium < High < Critical |
| 6 | same::`test_severity_as_str_is_stable` | Wire-format labels are stable |
| 7 | same::`test_critical_codes_never_auto_triage` | Safety invariant: Critical is always operator-action |
| 8 | same::`test_every_severity_is_assigned_to_at_least_one_code` | No dead severity tiers |
| 9 | same::`test_display_matches_code_str` | `Display` produces the wire format |
| 10 | same::`test_all_list_length_matches_catalogue_size` | `all()` vector cannot drift from enum |
| 11 | same::`test_code_str_follows_expected_prefix_pattern` | No rogue prefixes |
| 12 | `crates/common/tests/error_code_rule_file_crossref.rs::every_error_code_variant_appears_in_a_rule_file` | Every variant has rule documentation |
| 13 | same::`every_rule_file_code_has_an_enum_variant` | Every rule code has an enum variant (2-entry allowlist for historical typos) |
| 14 | same::`every_runbook_path_exists_on_disk` | `runbook_path()` always resolves to a real file |
| 15 | `crates/common/tests/error_code_tag_guard.rs::every_error_macro_tagged_with_a_known_code_carries_code_field` | Every `error!` that mentions a code in its message MUST also have `code = ErrorCode::X.code_str()` |
| 16 | same::`tagged_prefix_set_is_non_empty` | Guard setup sanity |
| 17 | `crates/storage/tests/error_level_meta_guard.rs::flush_persist_broadcast_failures_must_use_error_level` | No flush/persist/drain failure may be logged at `warn!` |
| 18 | same::`phrases_list_is_non_empty_and_lowercase` | Guard setup sanity |
| 19 | `crates/app/src/observability.rs::test_histogram_buckets_are_non_empty_and_monotonic` | Prometheus `_duration_ns` buckets stay monotonic |
| 20 | same::`init_errors_jsonl_appender_creates_directory` | JSONL sink does the side-effect it promises |
| 21 | same::`sweep_errors_jsonl_retention_*` (4 tests) | 48h retention sweeper preserves fresh, deletes old, ignores unrelated, handles missing dir |

Running `cargo test --workspace` executes every one; CI blocks merge on
any failure.

## The `#![deny(unused_must_use)]` blanket

Every prod `lib.rs` carries:

```rust
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
```

Any dropped `Result`, `let _ = result`, `.unwrap()`, `.expect()` in prod
code fails the build. The `(not(test))` gate keeps test boilerplate clean.

## Schema self-heal at boot

`ensure_instrument_tables` runs idempotent `ALTER TABLE ADD COLUMN IF
NOT EXISTS` between `CREATE TABLE` and `DEDUP ENABLE` so that tables
created by earlier builds (before the 2026-04-17 lifecycle columns
arrived) auto-migrate without a one-shot SQL script. See
`crates/storage/src/instrument_persistence.rs::ensure_instrument_tables`.

Future schema changes MUST follow this pattern: `CREATE TABLE IF NOT
EXISTS` with the new full schema, then an `ALTER TABLE ADD COLUMN IF
NOT EXISTS` for every column that didn't exist in a previous release.
QuestDB ignores ADDs that already exist, so running every boot is free.

## The auto-triage flow (Phase 6+, upcoming)

```
errors.jsonl ──→ signature_hash (sha256 of code+module+truncated_message)
                 │
                 ↓
     .claude/triage/error-rules.yaml
                 │
    ┌────────────┼────────────┐
    ↓            ↓            ↓
Known + safe   Known but    Novel
(severity<     Critical     signature
Critical)
    │            │            │
Auto-fix    Escalate:    Open draft
(runbook    Telegram +   GitHub Issue
script)     SMS +        with full
            GitHub       context,
            Issue        ping operator
```

Claude Code `/loop 5m .claude/triage/claude-loop-prompt.md` polls the
summary file and drives the above flow.

## Canonical on-disk paths (DO NOT CHANGE without updating Loki/Alloy)

| Path | Purpose | Writer |
|------|---------|--------|
| `data/logs/app.YYYY-MM-DD.log` | full app log | existing rolling writer |
| `data/logs/errors.log` | WARN+ single file | existing |
| `data/logs/errors.jsonl.YYYY-MM-DD-HH` | ERROR JSONL hourly | Phase 2 — `tracing-appender 0.2.3` |
| `data/logs/errors.summary.md` | human/Claude-readable snapshot | Phase 5 — refreshed every 60s |
| `data/logs/auto-fix.log` | audit trail of auto-triage actions | Phase 6 |
| `.claude/triage/error-rules.yaml` | triage classifier | Phase 6 |
| `.claude/triage/claude-loop-prompt.md` | Claude-watches-logs runbook | Phase 7 |
| `.claude/state/triage-seen.jsonl` | edge-trigger dedup | Phase 6 |

## What future sessions MUST NOT do

1. **Do not re-audit WARN→ERROR for flush/persist/drain sites.** The 28
   phrases in `crates/storage/tests/error_level_meta_guard.rs` are
   ratcheted. Adding a new flush handler? The meta-guard tells you the
   pattern by example. Don't scan the codebase from scratch.
2. **Do not duplicate the ErrorCode enum.** If a new code is needed,
   add a variant + a rule-file mention in the SAME PR. The cross-ref
   test enforces both directions; running it shows the gap.
3. **Do not add a new dropped-Result site.** `unused_must_use` denies
   at compile time. If the build fails with that error, handle the
   Result — don't `#[allow(...)]` it.
4. **Do not change the canonical paths in the table above.** Downstream
   Alloy/Loki scrapers, the summary writer, and the triage hook all
   hard-code these.
5. **Do not introduce a new `warn!` on a flush/persist/drain failure.**
   The meta-guard regexes these phrases; violations fail the build.
   Use `error!` with a `code =` field.
6. **Do not log at ERROR without a `code =` field** if the message
   mentions a known code prefix (I-P*, OMS-*, WS-*, STORAGE-*, etc.).
   The tag-guard fails the build.

## Completion status of the Zero-Touch plan

- [x] **Phase 0** — 25 WARN→ERROR escalations, `unused_must_use` lint,
      schema self-heal, error-level meta-guard
- [x] **Phase 1** — ErrorCode enum (53 variants), 3 cross-ref tests,
      tag-guard meta-test, first 3 production sites migrated
- [x] **Phase 2** — errors.jsonl hourly-rotated appender, 48h retention
      sweeper, 6 new observability tests, fully wired in main.rs
- [ ] **Phase 3** — Re-add Loki + Alloy (blocked on QuestDB mem trim
      confirmation)
- [ ] **Phase 4** — AWS CloudWatch parity (deferred until instance
      provisioned; Terraform stays ready)
- [x] **Phase 5** — `errors.summary.md` writer (60s refresh,
      signature-hash grouping, lookback filter, 18 unit tests, wired
      as a background tokio task from main.rs) + `make errors-summary`
- [x] **Phase 6** — `.claude/triage/error-rules.yaml` (7 seed rules
      incl. clear-spill), `.claude/triage/claude-loop-prompt.md`,
      `error-triage.sh` shell hook, 3 auto-fix scripts, triage_rules_guard
      meta-test, `make triage-dry-run/triage-execute`
- [x] **Phase 7.1** — `/loop` runbook prompt (claude-loop-prompt.md);
      Phase 7.2 MCP server still pending
- [~] **Phase 8.1** — Common auto-fix scripts (restart-depth, refresh-
      instruments, clear-spill — all three scaffolded, refresh-instruments
      fully functional today); Phase 8.2 Lambda bridge deferred until
      AWS instance provisioned
- [x] **Phase 9.2** — `scripts/validate-automation.sh` + `make
      validate-automation` runs 20 end-to-end checks;
      Phase 9.1 Grafana Operator Health dashboard still pending
- [~] **Phase 10.1** — Zero-tick-loss alert guard (4 Prometheus alerts
      pinned, 7 source-invariant tests); Phase 10.2 sequence-hole
      detector and 10.3 chaos test still pending
- [ ] **Phase 11** — WS + QuestDB resilience SLAs (chaos tests nightly)
- [x] **Phase 12.1** — 100% line coverage threshold set in
      `quality/crate-coverage-thresholds.toml`, enforced by
      `scripts/coverage-gate.sh` in CI
- [x] **Phase 12.5** — O(1) hot-path ratchet: DHAT zero-alloc tests +
      `quality/benchmark-budgets.toml` (Criterion ≤10ns/50ns/100ns
      budgets, 5% regression gate, enforced by
      `scripts/bench-gate.sh`)
- [x] **Phase 12.6** — Boot-time/process-time self-check via the e2e
      chain test (`crates/app/tests/observability_chain_e2e.rs`)
      asserting error! → JSONL → summary.md in-process
- [ ] **Phase 12.2/12.3** — Tighten mutation (zero-survivor gate) +
      fuzz 24h duration bump (extensions of existing CI workflows)
- [x] ~~**Phase 12.4** `#![deny(warnings)]` workspace-wide~~ SKIPPED —
      future toolchain deprecation warnings would silently break
      prod builds; the targeted lints already in place
      (`unused_must_use`, `unwrap_used`, `expect_used`, `print_*`,
      `dbg_macro`, `let_underscore_must_use`) are strictly better.

## Commit pointers (so sessions can jump to a specific change)

Branch: `claude/debug-expired-update-error-p5jSL` (PR #276)

| Commit | What |
|--------|------|
| `ae5c855` | Schema self-heal `ALTER TABLE ADD COLUMN IF NOT EXISTS` |
| `b53bca6` | Tick flush WARN→ERROR (5 sites) |
| `0ca2cb6` | Candle + depth flush WARN→ERROR (9 sites) |
| `f37484d` | App/core/historical WARN→ERROR (8 sites) |
| `eea39f7` | `unused_must_use` lint + error-level meta-guard |
| `e4fdf95` | ErrorCode enum + cross-ref tests |
| `8d34421` | Tag-field migration + tag-guard meta-test |
| `cce7188` | `tracing-appender` dep + errors.jsonl foundation |
| `d942695` | Wire JSONL layer + retention sweeper into main.rs |
| `7b87ab2` | Phase 5 summary_writer + this architecture doc |
| `a3145e9` | Phase 6 triage YAML + auto-fix scripts + shell hook |
| `9e807ca` | Phase 12.6 observability_chain_e2e (+ flatten_event fix) |
| `275157a` | Phase 10.1 zero-tick-loss alert guard (7 pins) |
| `a81206f` | Phase 2.2 / 5.2 / 8.1 / 9.2 operator commands + 20-check validation |

## Trigger (auto-loaded paths)

This rule activates when editing:
- `crates/app/src/observability.rs`
- `crates/app/src/main.rs` (tracing subscriber section)
- `crates/common/src/error_code.rs`
- `crates/common/tests/error_code_*.rs`
- `crates/storage/tests/error_level_meta_guard.rs`
- `crates/storage/src/instrument_persistence.rs` (schema)
- Any `*.rs` file containing `error!(`, `warn!(`, `tracing::error!(`
- Any file under `data/logs/` or `.claude/triage/`
