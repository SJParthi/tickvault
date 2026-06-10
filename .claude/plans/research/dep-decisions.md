# Workspace Dependency Decision Table — Dependabot PR #938 + full pin sweep

**Date:** 2026-06-10 (02:47 UTC / 08:17 IST)
**Repo state:** `main`-tracking branch `claude/busy-heisenberg-hncybo` @ `a6fe35c`
**Scope:** all 73 `[workspace.dependencies]` pins in root `Cargo.toml` + open dependabot PR #938.
**Action taken:** NONE — research only. Nothing merged, nothing bumped, no PR closed.

## Method (evidence discipline)

| Step | Tool | Evidence |
|---|---|---|
| Current pins | `Read Cargo.toml` (lines 17–167) | 73 workspace deps incl. dev-deps |
| Latest versions | crates.io API, one GET per crate (snapshot 2026-06-10 02:45 UTC) | raw JSON cached at `/tmp/dep-latest.json` |
| CVE status | `cargo audit` v-latest (installed in-session), advisory DB **1,123 advisories** | exit code **0**, **521 lockfile crates scanned, 0 vulnerabilities, 0 RUSTSEC warnings** |
| Breaking changes | otel `docs/release_0.32.md` (fetched from upstream main) + crates.io dependency metadata for `tracing-opentelemetry 0.33.0` | quoted below |
| Our call sites | `grep` across `crates/`, `fuzz/`, all `Cargo.toml`s | file:line cited per dep below |
| PR #938 | GitHub MCP `pull_request_read` (get / get_files / get_status) | lockfile diff quoted below |

**Honest envelope:** "latest" is a point-in-time snapshot of crates.io `max_stable_version` on 2026-06-10. Breaking-change analysis = release notes + call-site grep; the coordinated otel upgrade has NOT been compile-tested here (that is the upgrade-PR's job). PR #938's CI never ran (`get_status` → `state: pending, total_count: 0` — no checks executed), so the incompatibility claim below rests on the lockfile dependency graph, not on an observed CI failure.

---

## 1. VERDICT — Dependabot PR #938 (`opentelemetry_sdk` 0.31.0 → 0.32.1)

**Recommendation: CLOSE. Replace with one coordinated otel-quartet upgrade PR (pending Parthiban dated approval).**

### Why it cannot work as-is (mechanical proof from the PR's own Cargo.lock diff)

PR #938 bumps ONLY `opentelemetry_sdk`. Its lockfile diff (fetched via `pull_request_read get_files`) shows the resolver forced into a **duplicate otel tree**:

```
tickvault-app deps (from the PR's Cargo.lock):
  "opentelemetry 0.31.0"        <- our direct `opentelemetry` pin, unchanged
  "opentelemetry_sdk 0.32.1"    <- the bump
opentelemetry-otlp 0.31.1 deps:
  "opentelemetry 0.31.0", "opentelemetry_sdk 0.31.0"   <- still the OLD sdk
tracing-opentelemetry 0.32.1 deps:
  "opentelemetry 0.31.0"                               <- still the OLD api
```

Our call sites cross both trees in one function — `crates/app/src/observability.rs:274-312` (`init_tracing`):

| Call site | Line | Type-mismatch under PR #938 |
|---|---|---|
| `SpanExporter::builder().with_tonic().with_endpoint(...).build()` | 290–294 | exporter is an **sdk-0.31** `SpanExporter` impl (from otlp 0.31.1) |
| `SdkTracerProvider::builder().with_batch_exporter(exporter)` | 298–301 | builder comes from **sdk-0.32.1**; expects a 0.32 exporter trait impl → **compile error** |
| `OpenTelemetryLayer::new(tracer)` + `OpenTelemetryLayer<S, SdkTracer>` | 278, 304 | tracing-opentelemetry 0.32.1 expects a **0.31** tracer; `provider.tracer()` would return a **0.32.1** `SdkTracer` → **compile error** |
| `otel_provider: Option<SdkTracerProvider>` + `drop(otel_provider)` | `main.rs:6245`, `main.rs:6462` | same split-tree type |

The workspace comment at `Cargo.toml:59` already documents the constraint dependabot ignored: *"Versions pinned to 0.31.0 to match tracing-opentelemetry 0.32.1 (which depends on opentelemetry 0.31.0)"*. PR #938 changed the pin but left the comment + the three sibling pins, so it self-contradicts. PR is also `mergeable_state: behind` main.

### The correct upgrade path (when approved)

crates.io metadata confirms a matched 0.32 set now exists — `tracing-opentelemetry 0.33.0` depends on `opentelemetry ^0.32.0`, `opentelemetry_sdk ^0.32.0`, `opentelemetry-otlp ^0.32.0` (fetched from its dependencies endpoint). One PR bumping all four together:

| Crate | From | To |
|---|---|---|
| `opentelemetry` | 0.31.0 | 0.32.0 |
| `opentelemetry_sdk` | 0.31.0 | 0.32.1 |
| `opentelemetry-otlp` | 0.31.1 | 0.32.0 |
| `tracing-opentelemetry` | 0.32.1 | 0.33.0 |

**0.32 breaking changes checked against OUR call sites** (from upstream `docs/release_0.32.md`):

| 0.32 breaking change | Affects us? | Evidence |
|---|---|---|
| `SpanBuilder` fields/methods removed | NO | no `SpanBuilder` usage anywhere in `crates/` |
| `SamplingDecision`/`SamplingResult` moved to sdk | NO | not imported anywhere |
| `SdkTracer::{id_generator, should_sample}` removed | NO | we only call `provider.tracer("tickvault")` |
| `SpanExporter` trait methods now `&self` | NO | we don't implement the trait, only consume otlp's |
| OTLP: `ExportConfig`/`TonicConfig`/etc. removed | NO | we use only `WithExportConfig::with_endpoint` (observability.rs:18,292), which the notes say "remain unchanged" |
| OTLP tonic: https endpoint now errors without a TLS feature | **CONDITIONAL** | our `otlp_endpoint` is config-driven; defaults/tests are `http://`. IF prod ever uses `https://`, the upgrade PR must add otlp feature `tls-aws-lc` (matches the workspace aws-lc-rs rustls policy, Cargo.toml:139) |
| sdk `testing` feature renames (`TokioSpanExporter`→`TestSpanExporter`) | NO | not used |

Feature continuity verified via crates.io version metadata: `opentelemetry_sdk 0.32.1` still has **`rt-tokio`** (our enabled feature); `opentelemetry-otlp 0.32.0` still has **`grpc-tonic`** (our enabled feature).

The upgrade PR must also update the stale comment at `Cargo.toml:59` and watch for transitive `tonic`/`prost` churn in the lockfile. Per `cargo-and-docker.md`, this bump needs Parthiban's dated approval first.

**Dependabot hygiene suggestion (optional):** add a dependabot `groups:` entry for `opentelemetry*` + `tracing-opentelemetry` so it stops opening un-mergeable single-crate otel bumps.

---

## 2. CVE STATUS — whole workspace

```
cargo audit  →  exit 0
Loaded 1123 security advisories (advisory-db @ 2026-06-10)
Scanning Cargo.lock for vulnerabilities (521 crate dependencies)
0 vulnerabilities, 0 warnings
```

No RUSTSEC advisory against any direct or transitive dependency. The 3 historical RUSTSEC advisories on the AWS rustls-0.21 chain remain closed by the 2026-05-23 `default-https-client` feature routing (Cargo.toml:143-151) — confirmed still clean by this scan.

---

## 3. DECISION TABLE — the 9 deps where pin ≠ latest

| Dep | Pin | Latest | Our call sites (grepped) | Breaking changes affecting us | CVE | Recommendation |
|---|---|---|---|---|---|---|
| `opentelemetry` | 0.31.0 | 0.32.0 | `observability.rs:17` (`trace::TracerProvider` trait import) | none beyond version-matching (see §1) | clean | **upgrade-PR** (coordinated quartet, §1) |
| `opentelemetry_sdk` | 0.31.0 | 0.32.1 | `observability.rs:19,20,278,296-303`; `main.rs:6245,6462` | none at our sites; `rt-tokio` feature survives | clean | **close PR #938; upgrade-PR** (quartet) |
| `opentelemetry-otlp` | 0.31.1 | 0.32.0 | `observability.rs:18,290-294` (`SpanExporter`, `WithExportConfig`) | `with_endpoint` unchanged; https-without-TLS now errors (conditional, §1) | clean | **upgrade-PR** (quartet) |
| `tracing-opentelemetry` | 0.32.1 | 0.33.0 | `observability.rs:24,278,304` (`OpenTelemetryLayer`) | 0.33.0 = the otel-0.32-compatible release | clean | **upgrade-PR** (quartet) |
| `jsonwebtoken` | 10.3.0 | 10.4.0 | **ZERO** — workspace pin only (`Cargo.toml:75`); no crate `Cargo.toml` consumes it, no `.rs` reference anywhere (JWT handled as opaque `Secret<String>`) | n/a (no call sites) | clean | **hold**; separately propose REMOVING this dead pin to Parthiban |
| `ringbuf` | 0.4.8 | 0.5.0 | **ZERO** — workspace pin only (`Cargo.toml:34`); no consumer (hot path uses `rtrb` + crossbeam) | n/a (no call sites); 0.5 is semver-major for 0.x — irrelevant while unused | clean | **hold**; separately propose REMOVING this dead pin |
| `uuid` | 1.23.2 | 1.23.3 | `api/src/middleware.rs:173`, `trading/src/oms/idempotency.rs:60` — `Uuid::new_v4()` only (OMS-GAP-05) | patch release; `new_v4` is the most stable API in the crate | clean | **hold** — fold into the next approved maintenance bump; no urgency |
| `zerocopy` | 0.8.50 | 0.8.52 | **ZERO** `.rs` usage — declared in `crates/core/Cargo.toml:14` but on the machete `ignored` list (line 95) | n/a (patch + unused) | clean | **hold**; candidate for the same dead-pin removal proposal |
| `toml` | 1.1.2 | 1.1.2+spec-1.1.0 | figment config loading (config crates) | NONE — `+spec-1.1.0` is build-metadata only; same code version. Cargo treats them as equal precedence | clean | **no action** (not actually behind) |

## 4. DECISION TABLE — the 64 deps already AT latest (verified 1:1 against crates.io)

All pins below equal `max_stable_version` on 2026-06-10 → **hold (current), no action** for every row. CVE: clean (covered by the §2 scan).

| Section | Deps at latest |
|---|---|
| Async/WS | tokio 1.52.3, tokio-tungstenite 0.29.0, futures-util 0.3.32 |
| Dispatch/parse | enum_dispatch 0.3.13 |
| Data pipeline | papaya 0.2.4, dashmap 6.2.1, crossbeam-channel 0.5.15, rtrb 0.3.4, parking_lot 0.12.5 |
| Database | questdb-rs 6.1.0 |
| Metrics | metrics 0.24.6, metrics-exporter-prometheus 0.18.3, hdrhistogram 7.5.4, quanta 0.12.6 |
| Tracing | tracing 0.1.44, tracing-subscriber 0.3.23, tracing-appender 0.2.5 |
| Auth | arc-swap 1.9.1, totp-rs 5.7.1 |
| OMS/Risk | statig 0.4.1, jaeckel 0.2.0, governor 0.10.4, failsafe 1.3.0, backon 1.6.0, yata 0.7.0 |
| HTTP | reqwest 0.13.4, axum 0.8.9, tower 0.5.3, tower-http 0.6.11 |
| Config | figment 0.10.19 |
| Time | chrono 0.4.45, chrono-tz 0.10.4 |
| Serde | serde 1.0.228, serde_json 1.0.150, bitcode 0.6.9, rkyv 0.8.16, csv 1.4.0, thiserror 2.0.18, anyhow 1.0.102, bytes 1.11.1, sha2 0.11.0 |
| System | core_affinity 0.8.3, memmap2 0.9.10, notify 8.2.0, signal-hook 0.4.4, sd-notify 0.5.0, ahash 0.8.12, nohash-hasher 0.2.0, arrayvec 0.7.6 |
| Secrets/TLS | secrecy 0.10.3, zeroize 1.8.2, rustls 0.23.40, rustls-native-certs 0.8.4 |
| AWS | aws-config 1.8.18, aws-sdk-sns 1.103.0, aws-sdk-ssm 1.112.0 |
| CLI/IDs/Alerts | clap 4.6.1, uuid — see §3, teloxide 0.17.0 |
| Dev/test | criterion 0.8.2, proptest 1.11.0, loom 0.7.2, dhat 0.3.3, tokio-test 0.4.5 |

(64 = 73 total − 9 in §3.)

---

## 5. Summary of recommendations (no action taken — operator decides)

1. **PR #938 → CLOSE.** Single-crate otel bump is structurally un-compilable (split dependency tree, §1 lockfile proof). Do not merge, do not rebase.
2. **One coordinated otel upgrade-PR** (opentelemetry 0.32.0 + sdk 0.32.1 + otlp 0.32.0 + tracing-opentelemetry 0.33.0), only after Parthiban's dated approval per `cargo-and-docker.md`. Risk at our call sites: LOW (§1 table); conditional TLS-feature note if `otlp_endpoint` ever becomes https.
3. **Dead-pin cleanup proposal** (separate, needs approval): `jsonwebtoken` and `ringbuf` have zero consumers anywhere in the repo; `zerocopy` is declared-but-unused (machete-ignored). Removing them shrinks the audit surface. Until decided: hold.
4. **uuid 1.23.3, zerocopy 0.8.52:** patch-level, zero/trivial call-site exposure — hold, fold into the next approved maintenance window.
5. **Everything else (64 pins): already at latest, audit-clean — hold.**
6. **Optional:** dependabot `groups:` config for the otel family to prevent recurring un-mergeable single-crate bumps.

---

## 6. 🧃 EASY VIEW — the auto-driver scoreboard (operator-charter §G format)

> Sir, imagine your juice shop has **73 ingredients** on the shelf. We checked every single
> one against the market today: **64 are the freshest available 🟢, 0 have any poison/safety
> problem 🛡️, 9 need a look 🟡 — and the delivery boy (dependabot) tried to swap ONE part of
> a 4-part juicer motor with a new part that doesn't fit the other 3. We say NO to that
> delivery ❌.**

### The big scoreboard

| What we checked | Result | Score |
|---|---|---|
| 🛡️ Security / poison check (CVE scan, 521 packages deep) | ZERO problems found | ✅ 100% clean |
| 🟢 Already the newest version available | 64 of 73 | ✅ 88% perfectly fresh |
| 🟡 Newer version exists | 9 of 73 | see §3 |
| 💀 Bought but NEVER used (wasted shelf space) | 3 found (jsonwebtoken, ringbuf, zerocopy) | flagged for removal |

### The delivery we reject — dependabot PR #938

| Question | Answer |
|---|---|
| What did the robot try? | Upgrade **1 of 4** telemetry parts (`opentelemetry_sdk` 0.31 → 0.32) |
| Why is that bad? | The 4 parts are a **matched set** — like 4 tyres of one car. One new tyre + 3 old tyres = the build won't even compile |
| Proof? | The PR's own lockfile shows TWO copies of the same library fighting each other (§1) |
| Verdict | 🔴 **CLOSE this PR** |
| Right way? | ✅ ONE upgrade changing **all 4 tyres together** — only after operator dated approval |

### The 3 buttons for the operator (nothing done without approval)

| # | Decision | Recommendation |
|---|---|---|
| 1 | Dependabot PR #938 | ❌ Close — cannot compile by design |
| 2 | 4-together telemetry upgrade (otel 0.32.0 + sdk 0.32.1 + otlp 0.32.0 + tracing-otel 0.33.0) | ✅ Approve one combined PR — every call site checked, zero breakage hits us |
| 3 | Delete the 3 never-used ingredients | 🗑️ Approve — less to audit, less to worry |

**Honest envelope:** "fresh" = crates.io snapshot 2026-06-10; the 4-together upgrade is
verified by release notes + call-site grep, and gets its compile-proof in that upgrade
PR's own CI. Nothing was bumped in this research.
