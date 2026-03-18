# .claude/ — Claude Code Configuration

> This folder configures Claude Code's behavior for the dhan-live-trader project.
> Every file and folder here is auto-discovered by Claude Code at specific paths — do not reorganize.

---

## Quick Navigation

| Folder / File | What It Does |
|---------------|-------------|
| [`rules/`](rules/) | **31 rules in 2 folders: dhan/ (21 API) + project/ (10 general)** — auto-loaded when editing matching file paths |
| [`hooks/`](hooks/) | **18 hook scripts** — run automatically on commits, pushes, PRs |
| [`commands/`](commands/) | **6 slash commands** — `/status`, `/grill`, `/commit-push`, etc. |
| [`agents/`](agents/) | **3 specialized agents** — code review, hot-path audit, build verify |
| [`skills/`](skills/) | **2 skills** — phase status, quality gates |
| [`plan.md`](plan.md) | Active implementation plan (OMS Block 10) |
| [`settings.json`](settings.json) | Project-level Claude Code settings |
| [`plugins.json`](plugins.json) | Plugin registry |
| [`statusline.sh`](statusline.sh) | Terminal statusline display script |
| `settings.local.json.example` | Template for local settings overrides |

---

## Rules (`rules/`)

Rules auto-load when you edit files matching their `paths:` frontmatter. No manual activation needed.

```
rules/
├── dhan/                      ← 21 Dhan V2 API enforcement rules
│   ├── api-introduction.md
│   ├── authentication.md
│   ├── live-market-feed.md
│   └── ...18 more
│
└── project/                   ← 10 general project rules
    ├── rust-code.md
    ├── hot-path.md
    ├── testing.md
    └── ...7 more
```

### Dhan V2 API Rules (`rules/dhan/` — 21 rules)

Each rule points to its ground truth in `docs/dhan-ref/` and enforces correct enum values, byte offsets, and API contracts.

| Rule | Ground Truth | Scope |
|------|-------------|-------|
| [api-introduction](rules/dhan/api-introduction.md) | `docs/dhan-ref/01-*` | Base URLs, rate limits |
| [authentication](rules/dhan/authentication.md) | `docs/dhan-ref/02-*` | Token lifecycle |
| [live-market-feed](rules/dhan/live-market-feed.md) | `docs/dhan-ref/03-*` | WebSocket binary protocol |
| [full-market-depth](rules/dhan/full-market-depth.md) | `docs/dhan-ref/04-*` | 20/200-level depth |
| [historical-data](rules/dhan/historical-data.md) | `docs/dhan-ref/05-*` | Candle REST endpoints |
| [option-chain](rules/dhan/option-chain.md) | `docs/dhan-ref/06-*` | Option chain REST |
| [orders](rules/dhan/orders.md) | `docs/dhan-ref/07-*` | Order CRUD operations |
| [super-order](rules/dhan/super-order.md) | `docs/dhan-ref/07a-*` | Bracket/cover orders |
| [forever-order](rules/dhan/forever-order.md) | `docs/dhan-ref/07b-*` | GTT/GTC orders |
| [conditional-trigger](rules/dhan/conditional-trigger.md) | `docs/dhan-ref/07c-*` | Price/indicator triggers |
| [edis](rules/dhan/edis.md) | `docs/dhan-ref/07d-*` | Delivery instruction slip |
| [postback](rules/dhan/postback.md) | `docs/dhan-ref/07e-*` | Webhook callbacks |
| [annexure-enums](rules/dhan/annexure-enums.md) | `docs/dhan-ref/08-*` | All enums and status codes |
| [instrument-master](rules/dhan/instrument-master.md) | `docs/dhan-ref/09-*` | CSV instrument list |
| [live-order-update](rules/dhan/live-order-update.md) | `docs/dhan-ref/10-*` | Order status WebSocket |
| [market-quote](rules/dhan/market-quote.md) | `docs/dhan-ref/11-*` | LTP/OHLC REST |
| [portfolio-positions](rules/dhan/portfolio-positions.md) | `docs/dhan-ref/12-*` | Holdings/positions |
| [funds-margin](rules/dhan/funds-margin.md) | `docs/dhan-ref/13-*` | Fund limits/margin |
| [statements](rules/dhan/statements.md) | `docs/dhan-ref/14-*` | Trade history/P&L |
| [traders-control](rules/dhan/traders-control.md) | `docs/dhan-ref/15-*` | Kill switch |
| [release-notes](rules/dhan/release-notes.md) | `docs/dhan-ref/16-*` | API changelog |

### General Project Rules (`rules/project/` — 8 rules)

| Rule | Triggers On | Purpose |
|------|------------|---------|
| [rust-code](rules/project/rust-code.md) | `crates/**/*.rs` | Error handling, no `.unwrap()`, code style |
| [hot-path](rules/project/hot-path.md) | `websocket/**`, `pipeline/**`, `trading/**` | Zero-allocation enforcement |
| [data-integrity](rules/project/data-integrity.md) | `trading/**`, `storage/**`, `pipeline/**` | Idempotency, dedup keys |
| [testing](rules/project/testing.md) | `**/tests/**`, `**/*test*` | Test standards, naming, coverage |
| [market-hours](rules/project/market-hours.md) | `pipeline/**`, `trading/**`, `instrument/**` | IST market session rules |
| [cargo-and-docker](rules/project/cargo-and-docker.md) | `**/Cargo.toml`, `**/Dockerfile*` | Dependency pinning, container rules |
| [enforcement](rules/project/enforcement.md) | `.claude/hooks/**` | Hook architecture documentation |
| [gap-enforcement](rules/project/gap-enforcement.md) | `crates/**/*.rs` | Instrument gap mechanical rules |

## Hooks (`hooks/`)

Automated scripts that run at specific lifecycle points.

| Hook | When It Runs | What It Does |
|------|-------------|-------------|
| `block-env-files.sh` | Before file edit | Prevents `.env` file creation |
| `pre-commit-gate.sh` | Before commit | fmt, clippy, test on changed crates |
| `pre-push-gate.sh` | Before push | 7 fast quality gates (CI handles heavy checks) |
| `pre-pr-gate.sh` | Before PR | Branch check, naming, clean tree |
| `pre-merge-gate.sh` | Before merge | Full workspace validation |
| `test-count-guard.sh` | Quality gate | Ensures test count doesn't decrease |
| `pub-fn-test-guard.sh` | Quality gate | Every public function needs a test |
| `banned-pattern-scanner.sh` | Quality gate | Blocks unsafe patterns |
| `secret-scanner.sh` | Quality gate | Detects leaked secrets |
| `data-integrity-guard.sh` | Quality gate | Validates data contracts |
| `dedup-latency-scanner.sh` | Quality gate | Checks dedup + latency budgets |
| `financial-test-guard.sh` | Quality gate | Extra rigor for financial code |
| `session-sanity.sh` | Session start | Verifies environment is sane |
| `auto-save-remote.sh` | Background | Auto-pushes WIP to remote |
| `auto-save-watchdog.sh` | Background | Monitors auto-save health |
| `recover-wip.sh` | Recovery | Recovers from interrupted sessions |
| `plan-verify.sh` | Plan verification | Validates plan items are complete |
| `pre-tool-dispatch.sh` | Before tool use | Routes to correct hook |

---

## Commands (`commands/`)

Slash commands you can invoke in Claude Code with `/command-name`.

| Command | What It Does |
|---------|-------------|
| [`/status`](commands/status.md) | One-glance project dashboard |
| [`/grill`](commands/grill.md) | Adversarial code review — find every issue |
| [`/commit-push`](commands/commit-push.md) | Stage, commit (conventional), and push |
| [`/review-changes`](commands/review-changes.md) | Review uncommitted changes against rules |
| [`/test-and-fix`](commands/test-and-fix.md) | Run tests, analyze failures, fix until green |
| [`/worktree`](commands/worktree.md) | Create isolated git worktree for parallel work |

---

## Agents (`agents/`)

Specialized review agents invoked via `@agent-name`.

| Agent | Model | Purpose |
|-------|-------|---------|
| [`code-simplifier`](agents/code-simplifier.md) | Opus | Find reuse opportunities, dead code, complexity |
| [`hot-path-reviewer`](agents/hot-path-reviewer.md) | Opus | Audit hot-path code for allocation violations |
| [`verify-build`](agents/verify-build.md) | Haiku | Run full build pipeline (fmt → clippy → test) |

---

## Skills (`skills/`)

| Skill | Purpose |
|-------|---------|
| [`phase`](skills/phase/SKILL.md) | Read current phase doc and summarize status |
| [`quality`](skills/quality/SKILL.md) | Run all 8 quality gates matching CI |
