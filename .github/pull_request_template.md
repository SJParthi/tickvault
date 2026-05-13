## Summary

<!-- What changed and why? 1-3 bullet points. -->

-

## Type of Change

<!-- Check all that apply -->

- [ ] `feat` — New feature or capability
- [ ] `fix` — Bug fix
- [ ] `refactor` — Code restructure, no behavior change
- [ ] `test` — Adding or updating tests
- [ ] `docs` — Documentation changes
- [ ] `chore` — Build, CI, dependency updates
- [ ] `perf` — Performance improvement
- [ ] `security` — Security fix or hardening

## Quality Gates

<!-- All must pass before merging -->

- [ ] `cargo fmt --check` — Zero formatting issues
- [ ] `cargo clippy -- -D warnings` — Zero warnings
- [ ] `cargo test --workspace` — 100% pass
- [ ] `cargo audit` — Zero known CVEs
- [ ] No secrets in code (SSM Parameter Store only)
- [ ] No hardcoded values (constants or config only)
- [ ] No `.unwrap()` in production code
- [ ] Doc comments on all public items

## Testing Done

<!-- How was this tested? What scenarios? -->

-

## CLAUDE.md Compliance

- [ ] Versions from `Cargo.toml` workspace pin only (Bible V6 deleted S6-Step8)
- [ ] No banned crates or patterns
- [ ] Zero allocation on hot path (if applicable)
- [ ] O(1) or fail at compile time

## Operator Charter Compliance (see `.claude/rules/project/operator-charter-forever.md`)

### 15-row "100% everything" matrix — verify each item

- [ ] **Code coverage** — adds tests; coverage delta ≥ 0%
- [ ] **Audit coverage** — adds/extends a `<event>_audit` table with DEDUP UPSERT KEYS (if new event type)
- [ ] **Testing coverage** — declares which of the 22 test categories apply (unit/integration/property/loom/dhat/fuzz/etc.)
- [ ] **Code checks** — all 8 pre-commit + 11 git-hook + 12 pre-push gates green
- [ ] **Code performance** — DHAT zero-alloc + Criterion bench if hot path; ≤5% regression vs budget
- [ ] **Monitoring** — adds Prom counter + gauge + tracing span (7-layer telemetry)
- [ ] **Logging** — every error path uses `error!` with `code = ErrorCode::X.code_str()`
- [ ] **Alerting** — adds Prom alert rule in `alerts.yml` if new failure mode
- [ ] **Security** — `Secret<T>` for sensitive data; security-reviewer agent passed
- [ ] **Security hardening** — static IP / secret-scan / `unused_must_use` invariants intact
- [ ] **Bug fixing** — adversarial 3-agent review run (hot-path + security + hostile general-purpose)
- [ ] **Scenarios covering** — chaos test added if new failure mode
- [ ] **Functionalities covering** — every new pub fn has test + call site
- [ ] **Code review** — 3-agent review on diff (before AND after impl)
- [ ] **Extreme check** — ratchet test added that fails build on regression

### 7-row "Resilience" matrix — verify resilience demands intact

- [ ] **Zero ticks lost** — no new tick-drop path introduced
- [ ] **WS never disconnects** — `SubscribeRxGuard` + pool watchdog unchanged
- [ ] **Never slow/locked/hanged** — no new hot-path allocation (DHAT clean)
- [ ] **QuestDB never fails** — schema self-heal pattern preserved
- [ ] **O(1) latency** — Criterion bench unchanged (hot path)
- [ ] **Uniqueness + dedup** — composite `(security_id, exchange_segment)` enforced
- [ ] **Real-time proof** — 7-layer telemetry pinned by ratchet

### Telegram message style (if PR adds/changes any `NotificationEvent`)

- [ ] Plain English (no library jargon — banned word list in `topic-telegram-message-style-rules.md`)
- [ ] No file paths / no version numbers in body
- [ ] Severity emoji at start of subject
- [ ] Auto-driver litmus passed (60-year-old non-coder can read in 5 sec)
- [ ] Action verbs on degraded/critical ("What you need to do RIGHT NOW: 1...2...3...")
- [ ] One Telegram = one decision

### Honest 100% claim — if PR-body mentions "100%"

- [ ] Wording matches mandated envelope qualifier (see `operator-charter-forever.md` §F)
- [ ] No "never disconnect" / "never fails" lies without envelope
