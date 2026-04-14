# Testing Tooling Decisions

> **Authority:** `CLAUDE.md` > `.claude/rules/project/testing.md` > this file.
> **Purpose:** Record the tools that were considered and explicitly rejected for
> tickvault's testing stack, along with the reasoning. This saves future
> sessions from re-asking "should we add X?".

## Decisions

### Playwright — **REJECTED** (2026-04-14)

**Question:** should tickvault adopt Playwright for real-time UI testing?

**Answer:** No. tickvault has no meaningful UI surface to test.

**Reasoning:**

1. **No UI exists.** The only browser-accessible surface is the read-only
   `/portal` static page served by `crates/api/src/handlers/static_file.rs`.
   It is a HTML+JS view over the HTTP API, not an interactive trading UI.
   Orders are placed by the strategy engine via the Dhan REST API, not by a
   human clicking buttons.

2. **API-level tests already cover the portal.** The HTTP endpoints the portal
   reads (`GET /api/quote`, `GET /api/stats`, `GET /api/top-movers`,
   `GET /api/option-chain`, `GET /api/pcr`, `GET /api/index-constituency`)
   have integration tests in `crates/api/tests/`. Playwright would add a
   redundant layer of browser-based assertions over the same endpoints.

3. **CI cost does not justify the coverage.** Playwright needs a headless
   browser (Chromium), WebDriver, a display buffer, and ~1-2 GB of container
   space. On our current c7i.xlarge AWS budget (`aws-budget.md`) that
   overhead is real money for zero incremental defect catching.

4. **Browser tests are flaky.** Timing-dependent assertions in a browser are
   a notorious source of CI flakes. Tickvault is a HFT latency-sensitive
   system — we need test signals that are deterministic, not probabilistic.

5. **Future UI work does not live here.** If Parthiban ever adds a real
   trading UI (order ticket, position ladder, strategy controls), it will
   be in a separate repo with its own test stack. Keeping tickvault's CI
   pipeline lean lets the trading core stay fast.

**If Parthiban ever overrides this:** add Playwright only to a dedicated
`ui-tests/` workspace, gate it behind a CI job that can be skipped when the
portal files have not changed, and never add it to the pre-commit or pre-push
pipeline.

### Security scanning — **ALREADY COVERED** (2026-04-14)

tickvault already runs:

- `cargo audit` in CI (`Security & Audit` job) — blocks on CVE
- `cargo deny` in CI — blocks on banned licenses, duplicate versions, yanked
- Secret scan at commit (`.claude/hooks/secret-scanner.sh`) — blocks `.env`,
  AWS keys, private keys, JWT-like tokens
- Banned-pattern scanner (`.claude/hooks/banned-pattern-scanner.sh`) — blocks
  `f64::from(f32)` in storage, hot-path allocations, `unwrap`/`expect` in
  production code
- Pre-push secret scan re-run on the full diff
- GitHub Dependabot alerts on the default branch

Additional third-party security tools (Snyk, SonarQube, OWASP ZAP, etc.) add
cost without closing any gap that the above pipeline does not already cover.
The existing stack is the authoritative answer.

### What tickvault DOES use for testing

See `.claude/rules/project/testing.md` and `docs/standards/22-test-types.md`
for the canonical list. In short:

- **Unit** — `#[test]` in every `src/` module
- **Integration** — `crates/*/tests/`
- **Property** — `proptest` (random input robustness)
- **Concurrency** — `loom` (data race detection)
- **Zero-allocation** — `dhat` (hot-path allocation verification)
- **Chaos** — ignored-by-default tests that spawn real QuestDB / kill real
  processes and assert zero data loss
- **Fuzz** — `cargo-fuzz` (binary protocol crash testing)
- **Mutation** — `cargo-mutants` in weekly CI
- **Sanitizers** — AddressSanitizer + ThreadSanitizer in weekly CI
- **Coverage** — `cargo llvm-cov` with per-crate thresholds in
  `quality/crate-coverage-thresholds.toml`

Each of these catches a defect class Playwright cannot.

## Update History

- **2026-04-14** — initial decision, session 8. Playwright rejected. Security
  scanning documented as already-covered.
