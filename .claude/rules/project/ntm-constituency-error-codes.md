# NTM Constituency — Error Codes (NTM Sub-PR #10)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C > this file.
> **Companion code:** `crates/common/src/error_code.rs::ErrorCode::NtmConstituency01SourceDegraded`.
> **Companion contract:** `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §31 + §31.1.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to mention every `NtmConstituency*` variant.

---

## §0. Why this code exists

§31 (operator lock 2026-06-06) makes the live subscription the **NTM union** — indices + F&O
underlyings + the ~750 NIFTY Total Market constituent stocks. The constituent list comes from a
**second external source**, `niftyindices.com`
(`https://www.niftyindices.com/IndexConstituent/ind_niftytotalmarket_list.csv`), separate from the
Dhan instrument master.

Sub-PR #10 turns this on at boot: fetch the niftyindices CSV → `parse_constituents` →
`IndexConstituencyMap` → `resolve_constituents` against the Dhan master → bridge each resolved
`security_id` back to its Dhan `CsvRow` → `build_daily_universe(ntm_constituents)`.

**The failure policy (operator AskUserQuestion 2026-06-06 — "Degrade + alert"):** the Dhan CSV path
stays **fail-closed** (§4 infinite-retry, boot BLOCKS without a valid Dhan master). The
**niftyindices NTM layer is best-effort** — if it fails, boot **PROCEEDS** on the indices +
F&O-underlyings core universe and the operator is paged. Better to trade the core universe than to
halt all trading because a *secondary* list source is down.

---

## §1. NTM-CONSTITUENCY-01 — constituent source degraded

**Severity:** Critical. **Auto-triage:** No (operator must know today runs without the full NTM union).

**Trigger:** at boot, the NTM constituent layer could not be applied. Sub-causes:
1. **Fetch failure** — `ConstituencyDownloader::fetch_slug("ind_niftytotalmarket_list")` returned
   `Err` (network timeout, DNS, 5xx, redirect-blocked, body-cap exceeded — Sub-PR #3 hardening).
2. **Parse failure** — niftyindices returned 200 but the body was malformed / non-UTF-8 / missing a
   mandatory column (`parse_constituents` error).
3. **Resolve failure** — `> 2%` of constituents did not resolve to a Dhan NSE-EQ row
   (threshold raised 0.5% → 2% by operator lock 2026-06-08 — see §31.1(4); a few
   stragglers no longer nuke the whole list, they are skipped + logged by name)
   (`constituent_resolver::ConstituentResolveError::TooManyDangling`).

**Consequence (degrade, NOT halt):** `build_daily_universe` is called with `ntm_constituents` empty,
so the universe is the indices + F&O-underlyings core set (the proven pre-#10 ~250-SID universe).
The ~500 cash-only NTM constituents are absent for the day; F&O underlyings (which overlap most of
NTM) are unaffected. Trading continues on the core universe.

> **NOT degraded by this code:** an NTM set that pushes the universe past
> `MAX_DAILY_UNIVERSE_SIZE = 1200` is a *data anomaly* and HALTS via `INSTR-FETCH-04`
> (fail-closed) — that is a different, intentional path.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the `NTM-CONSTITUENCY-01` payload `reason` field
   (fetch / parse / dangling) + `core_universe_size`.
2. **Fetch reason:** `curl -sI https://www.niftyindices.com/IndexConstituent/ind_niftytotalmarket_list.csv`
   — is niftyindices.com up? If down, this self-resolves on the next trading day's boot.
3. **Parse reason:** download the file and inspect the header/rows — niftyindices may have changed
   the schema (open a follow-up to update `parse_constituents`).
4. **Dangling reason:** the niftyindices symbols/ISINs are out of sync with the Dhan master (NSE
   corporate action timing). Usually self-heals next day; if sustained, cross-check the §31.1
   ISIN-primary join.
5. No operator action restores TODAY's NTM constituents (degrade is for the session); the core
   universe is live and correct.

**Auto-triage safe:** NO (Critical — operator must be aware the day runs on the core universe).

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::NtmConstituency01SourceDegraded`
- `crates/core/src/instrument/daily_universe_orchestrator.rs` (degrade path)
- `crates/app/src/daily_universe_boot.rs` (async niftyindices fetch + degrade wiring)
- Alert path: `error!(code = NtmConstituency01SourceDegraded.code_str(), reason, core_universe_size)`
  at the degrade site → Telegram via the 5-sink error pipeline (a dedicated typed
  `NotificationEvent` is a deferred polish follow-up).

---

## §2. Cross-reference

| Component | File |
|---|---|
| Constituency downloader / parser / cache (#3) | `crates/core/src/instrument/index_constituency/` |
| ISIN→security_id resolver (#4) | `crates/core/src/instrument/constituent_resolver.rs` |
| Universe assembly + role flags (#5) | `crates/core/src/instrument/daily_universe.rs` |
| §31 lock | `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` |

---

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `NtmConstituency*` variant)
- `crates/core/src/instrument/daily_universe_orchestrator.rs`
- `crates/app/src/daily_universe_boot.rs`
- Any file containing `NTM-CONSTITUENCY-` or `NtmConstituency0`
