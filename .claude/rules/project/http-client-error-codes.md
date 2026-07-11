# Panic-Free HTTP Client Construction — Error Codes (HTTP-CLIENT-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file.
> **Companion code:** `crates/storage/src/http_client.rs`
> (`client_from_build_result` / `build_probe_client` / `shared_probe_client`),
> `crates/common/src/error_code.rs::ErrorCode::HttpClient01BuildFailed`.
> **Companion rules:** `wave-3-d-error-codes.md` (SLO-03 — the incident this
> closes the root cause of), `wave-4-error-codes.md` (RESOURCE-01 — the fd
> early-warning that should fire BEFORE this code does).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `HttpClient01*` variant verbatim —
> `HttpClient01BuildFailed` / `HTTP-CLIENT-01` appear below.

---

## §0. Why this code exists (the 2026-07-03 10:35 IST silent SLO-publisher death)

Eight sites in `crates/storage/src/` built a fresh `reqwest::Client` with the
fallback `Client::builder()...build().unwrap_or_else(|_| Client::new())`.
`reqwest::Client::new()` **PANICS** when the client cannot be constructed —
TLS backend init failure, DNS resolver init failure, fd exhaustion. Those are
the exact conditions under which the builder's `Err` arm becomes reachable,
so the "fallback" converted a recoverable error into a guaranteed panic. A
panic inside a tokio task is a **SILENT task death**: no `error!` line (the
panic prints to stderr, not the tracing pipeline), no respawn signal, no
Telegram.

The prime suspect site was `boot_probe.rs::wait_for_questdb_ready`, which is
NOT boot-only: it is invoked on the **every-10s SLO scheduler tick** and the
**every-5s pool-watchdog tick**, building a fresh client (full TLS + resolver
init) on every invocation — thousands of times per session. During the
2026-07-03 1.13M-frame storm the SLO publisher died silently at 10:35 IST
(the SLO-03 incident); fd/resolver exhaustion under that storm makes the
builder-failure → `Client::new()` panic → silent-task-death chain real.

The C2 fix: the repeating probes share ONE process-wide client
(`shared_probe_client`, a `OnceLock` — O(1) atomic load after first call),
and every former panic-fallback site now degrades through a typed
`HttpClientBuildError` with a loud `error!` + counter.

---

## §1. HTTP-CLIENT-01 — reqwest client build failed

**Severity:** High. **Auto-triage safe:** Yes (the degrade already happened —
the single probe/write was skipped, never the process; the operator inspects
the underlying resource pressure).

**Trigger:** `reqwest::ClientBuilder::build()` returned `Err` at one of the
storage HTTP-client sites — fd exhaustion (cannot create the resolver /
connection pool), DNS resolver init failure, or TLS backend init failure.
The site logs `error!(code = "HTTP-CLIENT-01", ...)`, increments
`tv_http_client_build_failed_total{site}` (STATIC site labels), and degrades:

| Site label | What degrades |
|---|---|
| `boot_probe` | one QuestDB readiness probe invocation returns `BootProbeError::ClientBuild` (typed) — the every-5s/10s scheduler retries next tick |
| `instrument_fetch_audit_ensure` | fetch-audit DDL skipped this boot (idempotent — next boot re-runs) |
| `shadow_ensure_tables` | candle-table DDL skipped this boot (idempotent) |
| `shadow_drop_legacy` | legacy candle cleanup skipped (marker not written — next boot retries) |
| `lifecycle_ensure` / `lifecycle_audit_ensure` | lifecycle DDL skipped this boot (idempotent) |
| `ticks_ensure_dedup` | ticks-table DDL skipped this boot (idempotent; ring/spill absorbs ILP errors) |
| `tick_gap_check` | one best-effort post-recovery gap check skipped |
| `named_views_ensure` | analyst console views (ticks_named/candles_named) DDL skipped this boot (idempotent — next boot re-runs; read-only projections, no data path affected, no duplicate-row window) |
| `wal_suspension_probe` | one 60s WAL-suspension probe tick skipped (W2 PR#6, 2026-07-10 — WAL-SUSPEND-01 watcher; next tick retries; probe-failed counter also rises) |

**Triage:**
1. `tv_http_client_build_failed_total{site}` — which site(s) and at what rate.
   A one-off at boot = transient; a sustained rate on `boot_probe` means the
   host is under resource pressure RIGHT NOW.
2. `ls /proc/<pid>/fd | wc -l` — fd exhaustion is the dominant cause;
   cross-check **RESOURCE-01** (`tv_open_fds` — the 80% early-warning that
   should have paged first) and the WS reconnect churn (WS-GAP-05).
3. Check resolver/TLS health: a storm of socket churn (frame storm, restart
   loop) can exhaust ephemeral state; correlate with
   `tv_ws_frame_spill_writer_respawn_total` and the SLO-03 respawn counter.
4. If the shared probe client failed at first build, the NEXT probe tick
   retries the build (the `OnceLock` is only initialized on success) — no
   restart needed once pressure subsides.

**Honest envelope:** the typed error degrades the SINGLE probe/write that
needed the client, never the process and never the hot tick path (all sites
are cold-path/boot/probe code). It does not prevent the underlying fd/TLS
exhaustion — RESOURCE-01/02 are the early-warning monitors for that; this
code makes the previously-silent panic loud and survivable.

**Honest envelope — DDL-skip duplicate-row window (flagged follow-up):** the
graceful `return` at the ensure-DDL sites (`ticks_ensure_dedup`,
`shadow_ensure_tables`, `lifecycle_ensure`, `lifecycle_audit_ensure`,
`instrument_fetch_audit_ensure`) means that if the target table does NOT
exist yet, QuestDB's ILP path may auto-create it WITHOUT DEDUP UPSERT KEYS —
a **duplicate-row window** lasting until the next successful boot re-runs the
idempotent DDL (which re-applies `DEDUP ENABLE UPSERT KEYS`; existing rows
written in the window may be duplicated and would need a manual dedup sweep).
Each site's `error!` message names this consequence explicitly. The
alternative — HALTING boot when the DDL client cannot be built — would be
MORE aggressive than the pre-C2 behaviour and is an **operator decision
(halt-vs-degrade) flagged as a follow-up**; until the operator rules, the
degrade-and-log behaviour stands. Note also: the `boot_probe` site's error
can repeat on every probe tick (every 5s pool-watchdog / 10s SLO scheduler)
while the FIRST shared-client build keeps failing — bounded, because the
`OnceLock` caches the client forever after the first success, and Telegram/
CloudWatch paging is alarm-driven (the triage rule carries a 300s cooldown),
not per-emission.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::HttpClient01BuildFailed`
- `crates/storage/src/http_client.rs` (shared client + typed error)
- `crates/storage/src/boot_probe.rs` (shared-client consumer +
  `BootProbeError::ClientBuild`)
- Degrade sites: `crates/storage/src/{instrument_fetch_audit_persistence,
  shadow_persistence, instrument_lifecycle_persistence, tick_persistence}.rs`
- Attribution branches (2026-07-03 review Fix 3): `crates/app/src/main.rs` +
  `crates/core/src/pipeline/tick_processor.rs` — a `BootProbeError::
  ClientBuild` from `wait_for_questdb_ready` logs HTTP-CLIENT-01 (host
  fd/TLS/resolver problem), NOT BOOT-02 (QuestDB runbook); control flow
  unchanged.
- Ratchet: `crates/storage/tests/http_client_fallback_guard.rs` — fails the
  build if any `Client::new()` / `Client::default()` fallback,
  `unwrap_or_else(|_| Client` pattern, or `use reqwest::Client as ` alias
  reappears in storage src, or if boot_probe stops using
  `shared_probe_client`. The guard's comment stripper treats `://` (URL
  scheme separators in string literals) as code, not a comment start, and
  self-tests that property.

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `HttpClient01*` variant)
- `crates/storage/src/http_client.rs`
- `crates/storage/src/boot_probe.rs`
- Any file containing `HTTP-CLIENT-01`, `HttpClient01`, `shared_probe_client`,
  `HttpClientBuildError`, or `tv_http_client_build_failed_total`
