# DhanHQ API v2 — BONUS: DhanHQ Agent Skills (Claude Code / SKILL.md Pack)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/skills/


---

## DhanHQ Agent Skills

Open SKILL.md skills that give your AI coding agent deep knowledge of DhanHQ — order placement, portfolio, market data, option chains, live feeds, and ScanX — with built-in safety guardrails.

**Dhan-native agent skills for NSE / BSE equities, F&O, and commodity trading.**

DhanHQ Agent Skills give your AI coding agent the ability to place live orders, read real portfolio data, stream market feeds, and access the full instrument universe of Indian exchanges — through [DhanHQ's APIs](https://api.dhan.co/v2/).

Built for the open [Agent Skills](https://agentskills.io) standard, the pack is compatible with **Claude Code**, **Codex**, and any agent that supports `SKILL.md`.

> Source of truth: **[github.com/dhan-oss/dhanhq-skills](https://github.com/dhan-oss/dhanhq-skills)**

---

## How it works

A *skill* is a small folder of Markdown and Python that an AI agent can load on demand. Each skill follows the open [`SKILL.md`](https://agentskills.io) format — frontmatter that tells the agent *when* to activate, followed by setup, safety rules, and runnable code patterns.

The DhanHQ skill pack uses **progressive disclosure** to keep your agent's context lean:

1. **`SKILL.md`** (~300 lines) — Setup, safety rules, constants, and core code patterns. Enough for ~80% of trading tasks.
2. **`references/*.md`** — Loaded only when a task needs deeper detail (full order parameter tables, WebSocket setup, Greeks math).
3. **`scripts/`** — Reusable utilities the agent can call directly (security resolver, order validator, trade journal).
4. **`examples/`** — Complete, runnable scripts the agent can reference or adapt (iron condor, super order, live feed, etc.).

Your agent reads the skill, gains DhanHQ-specific knowledge (exchange segments, instrument types, order varieties, lot sizes), and starts calling the right APIs with the right parameters.

---

## Install

### Global install

```bash
npm install -g skills
skills add dhan-oss/dhanhq-skills --skill dhanhq
```

### Claude Code or Codex (zero-install)

```bash
npx skills add dhan-oss/dhanhq-skills --skill dhanhq
```

or

```bash
npx @dhan-oss/dhanhq-skill
```

Once installed, your agent automatically gains DhanHQ capabilities — no further configuration.

See the [installation guide](/skills/getting-started/installation) for per-agent setup notes.

---

## Requirements

- **Python 3.8+** and `pip install dhanhq`
- **Order APIs** — static IP whitelisting on Dhan (one-time)
- **Data APIs** — active Dhan Data Plan
- **Credentials** — environment variables `DHAN_CLIENT_ID` and `DHAN_ACCESS_TOKEN`

The skill never hardcodes credentials. Your agent reads them from the environment at call time.

---

## What's included

```
skills/dhanhq/
├── SKILL.md                       Entry point — setup, safety, core patterns
│
├── references/                    Deep-dive docs, loaded on demand
│   ├── orders.md                  Regular, super, forever (GTT), AMO, slice orders
│   ├── portfolio.md               Holdings, positions, convert position, eDIS
│   ├── market-data.md             Historical OHLC, intraday, quotes
│   ├── option-chain.md            Option chain with Greeks, expiry list
│   ├── instruments.md             Security master, symbol resolution
│   ├── funds.md                   Fund limits and margin calculator
│   ├── live-feed.md               WebSocket — MarketFeed, OrderUpdate, FullDepth
│   ├── error-codes.md             Error codes, rate limits, retry patterns
│   ├── common-workflows.md        Multi-step patterns (rebalance, iron condor, P&L)
│   ├── options-analysis-patterns.md  PCR, max pain, payoff diagrams, IV skew
│   └── backtesting-with-dhan.md   Equity + F&O backtest patterns with cost model
│
├── scripts/
│   ├── dhan_helpers.py            Composable helper library
│   ├── resolve_security.py        Human name → security_id resolver
│   ├── validate_order.py          Pre-flight order validation with guardrails
│   └── trade_logger.py            Persistent trade journal
│
└── examples/
    ├── place_equity_order.py      Simple equity delivery order
    ├── place_fno_order.py         F&O option order with lot-size validation
    ├── fetch_option_chain.py      Nifty option chain with ATM analysis
    ├── iron_condor.py             Multi-leg strategy: build, analyze, place
    ├── super_order_with_sl.py     Entry + target + trailing SL in one order
    ├── gtt_forever_order.py       GTT single trigger and OCO orders
    ├── order_management.py        Full lifecycle — place, modify, cancel, book
    ├── portfolio_summary.py       Holdings + positions + funds dashboard
    ├── margin_check.py            Pre-order margin validation
    ├── historical_data_analysis.py  OHLCV + moving averages + stats
    └── live_feed_setup.py         WebSocket market data streaming
```

---

## When agents use this skill

The skill activates when the user:

- Wants to **place, modify, or cancel** stock or F&O orders
- Asks about **portfolio holdings or positions**
- Needs **live or historical market data** (OHLC, quotes, depth)
- Wants to work with **option chains** (Greeks, OI, IV)
- Asks about **fund limits or margin requirements**
- Mentions **DhanHQ**, **Dhan API**, or Indian stock market trading
- Wants to **build trading automation** for NSE / BSE / MCX
- Needs to **backtest a strategy** using historical data

---

## Built-in safety guardrails

Trading with AI agents needs hard guardrails. Every DhanHQ skill ships with them on by default:

| Rule | What it does |
|------|--------------|
| **Confirmation required** | Always shows order preview and asks for user confirmation before placing |
| **Default to LIMIT** | Never places MARKET orders unless the user explicitly requests them |
| **Default to 1 lot** | Defaults to 1 share (equity) or 1 lot (F&O) when quantity is unspecified |
| **Lot-size validation** | Rejects F&O orders whose quantity isn't a multiple of the exchange lot size |
| **Product-type guardrails** | Blocks CNC / MTF for F&O segments; blocks invalid product-segment combos |
| **Notional warning** | Warns when order value exceeds ₹50,000 |
| **Freeze quantity check** | Warns when F&O quantity exceeds exchange freeze limits |
| **Market-hours check** | Warns when the market is closed and suggests AMO |
| **No hardcoded tokens** | Always reads credentials from environment variables |

Your agent stays a knowledgeable assistant — not an autonomous trader. You stay in control of every order.

Full details: [Safety guardrails](/skills/getting-started/safety-guardrails).

---

## API coverage

| Category | Reference |
|----------|-----------|
| Orders — regular, super, forever (GTT), AMO, slice | [skills/dhanhq/references/orders.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/orders.md) |
| Portfolio — holdings, positions, convert, eDIS | [references/portfolio.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/portfolio.md) |
| Market data — historical OHLC, quotes, depth | [references/market-data.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/market-data.md) |
| Option chain — Greeks, OI, expiry list | [references/option-chain.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/option-chain.md) |
| Instruments — security master, symbol resolution | [references/instruments.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/instruments.md) |
| Funds & margin | [references/funds.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/funds.md) |
| Live feed — MarketFeed, OrderUpdate, FullDepth | [references/live-feed.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/live-feed.md) |
| ScanX real-time scanner | [references/scanx-data.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/scanx-data.md) |
| Error codes, rate limits, retry patterns | [references/error-codes.md](https://github.com/dhan-oss/dhanhq-skills/blob/main/skills/dhanhq/references/error-codes.md) |

---

## Example prompts

Once installed, drop these into your agent:

**Orders**
- *"Buy 10 shares of Reliance at market."*
- *"Place a limit order for HDFC Bank at 1650."*
- *"Buy 1 lot of Nifty 24000 CE expiry this week."*
- *"Place a super order on TCS with target 4200 and SL 3900."*
- *"Set a GTT to buy Infosys if it drops to 1400."*

**Portfolio**
- *"Show me my holdings."*
- *"What's my total portfolio value?"*
- *"Show my open F&O positions."*
- *"Convert my INFY position from intraday to delivery."*

**Market data**
- *"Get daily OHLC for Reliance for the last 6 months."*
- *"What's the current LTP of HDFC Bank?"*
- *"Show me 5-minute candles for TCS today."*

**Options**
- *"Show me the Nifty option chain for nearest expiry."*
- *"What's the PCR for Bank Nifty?"*
- *"Build me a Nifty iron condor."*

**Funds & margin**
- *"What's my available margin?"*
- *"How much margin do I need to sell 1 lot of Nifty?"*

---

## Agent compatibility

The pack works with any agent that reads the `SKILL.md` standard:

- **Claude Code** — Anthropic's coding agent
- **Codex** — OpenAI's coding agent
- **Cursor**, **Continue**, and others that adopt the standard

Same skill, same install, same behavior — the agent reads the spec, you get DhanHQ-aware tooling.

---

## SDK reference

- **Package** — `dhanhq` ([PyPI](https://pypi.org/project/dhanhq/))
- **Minimum version** — 2.2.0
- **Base URL** — `https://api.dhan.co/v2`
- **API docs** — [dhanhq.co/docs/v2](https://dhanhq.co/docs/v2/)
- **SDK source** — [github.com/dhan-oss/DhanHQ-py](https://github.com/dhan-oss/DhanHQ-py)

---

## Contributing

The skill is open source under MIT. To contribute:

1. Fork [dhan-oss/dhanhq-skills](https://github.com/dhan-oss/dhanhq-skills)
2. Make changes in `skills/dhanhq/`
3. Verify against the DhanHQ SDK (`pip install dhanhq`)
4. Submit a pull request

---

## Next steps

- [Installation](/skills/getting-started/installation) — Per-agent setup details
- [Safety guardrails](/skills/getting-started/safety-guardrails) — Full guardrail reference
- [What are skills?](/skills/overview/what-are-skills) — Conceptual overview
- [SKILL.md standard](/skills/overview/skill-standard) — File format spec

---

## The SKILL.md Standard

SKILL.md is an open standard for describing API capabilities in a format AI agents can understand and act upon.

SKILL.md is an open standard format for describing API capabilities in a way that AI coding agents can understand and act upon. It bridges the gap between traditional API documentation and AI-native interaction — giving agents the context they need to make correct API calls.

DhanHQ Skills are built entirely on this standard. Learn more at [agentskills.io](https://agentskills.io/).

## How It Works

A SKILL.md file is structured Markdown that contains:

- **API endpoint descriptions** with parameters, request/response formats, and examples
- **Domain knowledge** such as exchange segments, instrument types, and order varieties
- **Safety rules** that the agent follows to prevent unintended actions
- **Workflow recipes** for common multi-step operations
- **Error handling guidance** with troubleshooting steps

The agent reads these skill files as context and uses them to generate correct API calls, validate inputs, and guide you through complex workflows — all through natural conversation.

## DhanHQ Skill Structure

```
skills/
└── dhanhq/
    ├── SKILL.md                          # Entry point — setup, safety rules, core patterns
    │
    ├── references/                       # Deep-dive docs loaded on demand
    │   ├── orders.md                     # Order lifecycle (regular, super, forever, AMO)
    │   ├── portfolio.md                  # Holdings, positions, convert position, eDIS
    │   ├── market-data.md                # Historical OHLC, intraday, quotes
    │   ├── option-chain.md              # Option chain with Greeks, expiry list
    │   ├── instruments.md               # Security master, symbol resolution
    │   ├── funds.md                     # Fund limits and margin calculator
    │   ├── live-feed.md                 # WebSocket: MarketFeed, OrderUpdate, FullDepth
    │   ├── error-codes.md              # Error codes, rate limits, retry patterns
    │   ├── common-workflows.md         # Multi-step patterns (rebalance, iron condor, P&L)
    │   ├── options-analysis-patterns.md # PCR, max pain, payoff diagrams, IV skew
    │   └── backtesting-with-dhan.md    # Equity + F&O backtest patterns with cost model
    │
    ├── scripts/
    │   ├── dhan_helpers.py              # Composable helper library
    │   ├── resolve_security.py          # Human name → security_id resolver
    │   ├── validate_order.py            # Pre-flight order validation with guardrails
    │   └── trade_logger.py              # Persistent trade journal
    │
    └── examples/
        ├── place_equity_order.py        # Simple equity delivery order
        ├── place_fno_order.py           # F&O option order with lot-size validation
        ├── fetch_option_chain.py        # Nifty option chain with ATM analysis
        ├── iron_condor.py               # Multi-leg strategy: build + analyze + place
        └── ...                          # More examples in the repo
```

## Open Standard Benefits

- **Agent-agnostic** — Works with Claude Code, Codex, and any agent that supports the format
- **Portable** — Skills can be shared, forked, and contributed to via Git
- **Versionable** — Plain text files that work with standard version control
- **Human-readable** — Being Markdown, skills are easy to review and audit

## Learn More

- Full standard specification: [agentskills.io](https://agentskills.io/)
- DhanHQ Skills source: [github.com/dhan-oss/dhanhq-skills](https://github.com/dhan-oss/dhanhq-skills)
- Skills CLI: [github.com/vercel-labs/skills](https://github.com/vercel-labs/skills)

---

## What Are Agent Skills?

Agent Skills are reusable capabilities for AI agents — procedural knowledge that helps agents accomplish trading tasks on Indian exchanges.

Skills are reusable capabilities for AI agents. They provide procedural knowledge that helps agents accomplish specific tasks more effectively. Think of them as plugins or extensions that enhance what your AI agent can do.

The `skills` CLI that powers the ecosystem is open source at [github.com/vercel-labs/skills](https://github.com/vercel-labs/skills).

## How DhanHQ Skills Work

DhanHQ Skills give your AI agent the ability to place live orders, read real portfolio data, stream market feeds, and access the full instrument universe of Indian exchanges — all through [DhanHQ's APIs](https://api.dhan.co/v2/#/).

The skill follows **progressive disclosure** to minimize context usage:

1. **SKILL.md** (~300 lines) gives the agent setup, safety rules, constants, and core code patterns — enough for 80% of tasks.
2. **references/*.md** are loaded only when a task needs deeper detail (e.g., full order parameter tables, WebSocket setup).
3. **scripts/** provide reusable utilities the agent can call directly.
4. **examples/** are complete, runnable scripts the agent can reference or adapt.

## When Agents Use This Skill

The skill activates when you:

- Want to place, modify, or cancel stock or F&O orders
- Ask about portfolio holdings or positions
- Need live or historical market data (OHLC, quotes, depth)
- Want to work with option chains (Greeks, OI, IV)
- Ask about fund limits or margin requirements
- Mention DhanHQ, Dhan API, or Indian stock market trading
- Want to build trading automation for NSE/BSE/MCX
- Need to backtest a strategy using historical data

## Repository

DhanHQ Skills are open source: [github.com/dhan-oss/dhanhq-skills](https://github.com/dhan-oss/dhanhq-skills)

Built for the [Agent Skills open standard](https://agentskills.io/) and compatible with Claude Code, Codex, and any agent that supports SKILL.md.

## Next Steps

- Check [agent compatibility](/skills/overview/compatibility)
- Follow the [installation guide](/skills/getting-started/installation)
- Review [safety guardrails](/skills/getting-started/safety-guardrails)

---

## Agent Compatibility

DhanHQ Skills work with Claude Code, Codex, and any AI agent that supports the SKILL.md open standard.

DhanHQ Skills are built on the [SKILL.md open standard](/skills/overview/skill-standard), which means they work with any AI coding agent that can read structured Markdown context. Below is a list of compatible agents and what to expect from each.

## Supported Agents

### Claude Code

[Claude Code](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/overview) is Anthropic's agentic coding tool. It natively supports the SKILL.md format and can:

- Read skill files as context to generate correct DhanHQ API calls
- Follow safety rules defined in SKILL.md (order confirmations, lot size checks)
- Load reference files on demand for deeper task context
- Execute helper scripts and adapt examples to your use case

Claude Code is the primary development and testing target for DhanHQ Skills.

### Codex

[Codex](https://openai.com/index/introducing-codex/) from OpenAI supports SKILL.md-based skills. With DhanHQ Skills installed, Codex can:

- Understand DhanHQ API endpoints, parameters, and response formats
- Generate trading automation scripts using skill context
- Apply safety guardrails when constructing order payloads
- Reference domain knowledge (exchange segments, instrument types, order varieties)

### Any SKILL.md-Supporting Agent

The SKILL.md standard is open and agent-agnostic. Any AI agent that can:

1. Read Markdown files as context
2. Follow structured instructions within those files
3. Execute or reference code snippets

...is compatible with DhanHQ Skills. As the ecosystem grows, more agents will adopt the standard. Check [agentskills.io](https://agentskills.io/) for the latest list of supporting agents and tools.

## What Compatibility Means

When we say an agent is "compatible," it means:

| Capability | Description |
|-----------|-------------|
| **Context loading** | The agent reads SKILL.md and reference files to understand DhanHQ APIs |
| **Safety rule adherence** | The agent follows guardrails like order confirmation prompts and LIMIT order defaults |
| **Code generation** | The agent produces correct API calls using patterns from the skill |
| **On-demand references** | The agent loads deeper documentation only when a task requires it |

## Adding Support for New Agents

If you're building an AI agent or tool that reads Markdown context, you can support DhanHQ Skills by:

1. Implementing SKILL.md file discovery (look for `skills/` directories or use the `skills` CLI)
2. Loading the top-level `SKILL.md` as primary context
3. Loading files from `references/` when the task requires deeper detail
4. Respecting safety rules marked in the skill (e.g., `⚠️ SAFETY` blocks)

See the [SKILL.md standard](/skills/overview/skill-standard) page for the full format specification.

## Next Steps

- [Install DhanHQ Skills](/skills/getting-started/installation)
- [Configure authentication](/skills/getting-started/configuration)
- [Review safety guardrails](/skills/getting-started/safety-guardrails)

---

## Installation

Step-by-step guide to installing the Skills CLI and adding DhanHQ Agent Skills to your development environment.

This guide walks you through installing the Skills CLI and adding the DhanHQ skill pack so your AI agent can interact with Indian markets.

## Prerequisites

- **Node.js** v18 or later
- **npm** (comes with Node.js)
- A DhanHQ trading account with API access enabled

## Step 1: Install the Skills CLI

Install the Skills CLI globally using npm:

```bash
npm install -g skills
```

Verify the installation:

```bash
skills --version
```

## Step 2: Add the DhanHQ Skill Pack

Once the CLI is installed, add the DhanHQ skills:

```bash
skills add dhan-oss/dhanhq-skills --skill dhanhq
```

This downloads the skill pack from the [dhan-oss/dhanhq-skills](https://github.com/dhan-oss/dhanhq-skills) repository and registers it with your local agent environment.

## What Gets Installed

The skill pack includes:

- **SKILL.md** — Core knowledge file (~300 lines) with setup instructions, safety rules, constants, and code patterns
- **references/** — Detailed API parameter tables, WebSocket setup guides, and exchange-specific documentation
- **scripts/** — Reusable utility scripts your agent can call directly
- **examples/** — Complete, runnable trading scripts for common workflows

## Verifying the Installation

After installation, confirm the skill is available:

```bash
skills list
```

You should see `dhanhq` listed among your installed skills.

## Next Steps

- [Configure authentication](/skills/getting-started/configuration) to connect your DhanHQ account
- Review the [safety guardrails](/skills/getting-started/safety-guardrails) before placing live orders
- Explore [skill categories](/skills/) to see what your agent can do

---

## Configuration

Configure your DhanHQ access token so your AI agent can authenticate with DhanHQ APIs.

After installing the DhanHQ skill pack, you need to configure authentication so your AI agent can interact with DhanHQ APIs on your behalf.

## Access Token Setup

DhanHQ APIs authenticate using an **access token** passed as a header with every request. Your agent needs this token to place orders, fetch portfolio data, stream market feeds, and perform any trading operation.

### Step 1: Generate Your Access Token

1. Log in to [web.dhan.co](https://web.dhan.co)
2. Click on **My Profile** and navigate to **Access DhanHQ APIs**
3. Generate an **Access Token** (valid for 24 hours)
4. Copy the token — you'll need it in the next step

> **tip:** You can optionally provide a **Postback URL** while generating the token to receive real-time order updates. See the [Postback guide](/api/v2/guides/postback) for details.

### Step 2: Configure the Token for Your Agent

Set the access token as an environment variable so the skill can pick it up automatically:

```bash
export DHAN_ACCESS_TOKEN="your-access-token-here"
```

To persist this across terminal sessions, add it to your shell profile (`~/.bashrc`, `~/.zshrc`, or equivalent):

```bash
echo 'export DHAN_ACCESS_TOKEN="your-access-token-here"' >> ~/.zshrc
source ~/.zshrc
```

### Step 3: Verify the Configuration

Ask your agent to check the connection:

```text
Check my DhanHQ account profile
```

The agent will call the `/v2/profile` endpoint using your token. A successful response confirms authentication is working.

## Token Renewal

Access tokens generated from Dhan Web are valid for **24 hours**. When your token expires, generate a new one from [web.dhan.co](https://web.dhan.co) and update the environment variable.

For longer-lived sessions, you can use the API key-based authentication flow:

1. Generate an **API Key & Secret** from Dhan Web (valid for 12 months)
2. Use the [Generate Consent](/api/v2/authentication/api-key-generate-consent) and [Consume Consent](/api/v2/authentication/api-key-consume-consent) flow to obtain a token programmatically

See the full [Authentication guide](/api/v2/guides/authentication) for all available methods.

## Static IP Whitelisting

Per SEBI and exchange guidelines, order placement APIs require **Static IP whitelisting**. If your agent will place, modify, or cancel orders, you must whitelist your IP address:

1. Go to [web.dhan.co](https://web.dhan.co) → DhanHQ APIs section
2. Set your static IP address, or use the [Set IP API](/api/v2/authentication/set-ip)

> **note:** Static IP is only required for order-related operations (Orders, Super Order, Forever Order). Reading portfolio data, market data, and other non-order APIs do not require IP whitelisting.

## Client ID

Some API endpoints also require your **Client ID** (your Dhan account number). Set it alongside your access token:

```bash
export DHAN_CLIENT_ID="your-client-id"
```

Your Client ID is visible on [web.dhan.co](https://web.dhan.co) under My Profile.

## Security Best Practices

- **Never commit tokens to version control.** Use environment variables or a secrets manager.
- **Rotate tokens regularly.** Generate a fresh token daily or use the API key flow for automated renewal.
- **Use LIMIT orders by default.** The skill enforces this as a safety guardrail, but proper authentication ensures only you can execute trades.
- **Review agent actions.** The skill requires confirmation before placing orders — always verify the details before approving.

## Next Steps

- Review the [safety guardrails](/skills/getting-started/safety-guardrails) that protect your account during agent-assisted trading
- Explore [skill categories](/skills/) to see what your agent can do with a configured token
- Read the full [Authentication documentation](/api/v2/guides/authentication) for advanced auth flows

---

## Safety Guardrails

Understand the built-in safety mechanisms that protect your account when trading with AI agents using DhanHQ Skills.

Trading with AI agents introduces unique risks — a misinterpreted instruction or a missing parameter could result in unintended trades. DhanHQ Skills include built-in safety guardrails that keep you in control of every decision while still benefiting from agent-assisted workflows.

## Why Guardrails Matter

AI agents are powerful assistants, but they operate on natural language instructions that can be ambiguous. A request like "buy Nifty" could mean many things — which expiry, what strike, how many lots, at what price? Without guardrails, an agent might make assumptions that lead to:

- Orders placed at unfavorable market prices
- Incorrect lot sizes causing excessive exposure
- Trades executed without your explicit approval

DhanHQ Skills address these risks with three core safety mechanisms that work together to prevent accidental or unintended trades.

## Order Confirmation

**Every order action requires your explicit approval before execution.**

When your agent prepares to place, modify, or cancel an order, it presents the full order details and waits for your confirmation. No order is ever submitted silently.

### What Gets Confirmed

The agent will ask for confirmation before:

- **Placing a new order** — Shows instrument, exchange, order type, quantity, price, and transaction type (BUY/SELL)
- **Modifying an existing order** — Shows the original order details alongside the proposed changes
- **Cancelling an order** — Identifies the order and asks you to confirm cancellation

### Example Flow

```text
You: Buy 1 lot of Nifty 24500 CE expiring this week

Agent: I'll place the following order:
  • Instrument: NIFTY 24500 CE (weekly expiry)
  • Exchange: NSE_FNO
  • Transaction: BUY
  • Quantity: 25 (1 lot)
  • Order type: LIMIT
  • Price: ₹185.50 (current ask)

  Shall I proceed? (yes/no)
```

You review the details and confirm only when everything looks correct. If something is wrong, you can ask the agent to adjust before placing.

> **tip:** The confirmation step also serves as a learning opportunity — you can verify that the agent correctly interpreted your intent and understood the instrument, quantity, and pricing.

## LIMIT Order Defaults

**All orders default to LIMIT unless you explicitly request MARKET.**

MARKET orders execute immediately at the best available price, which can differ significantly from the last traded price — especially in volatile or illiquid instruments. LIMIT orders give you price control by specifying the maximum (for buys) or minimum (for sells) price you're willing to accept.

### Why LIMIT by Default

| Scenario | MARKET Order Risk | LIMIT Order Protection |
|----------|-------------------|----------------------|
| Volatile market open | Fill at a price far from previous close | Order waits at your specified price |
| Illiquid F&O strike | Wide bid-ask spread causes slippage | No fill unless your price is met |
| Flash crash / spike | Executes at extreme prices | Protects against outlier fills |
| Fat-finger quantity | Immediate execution of wrong size | Time to cancel before fill |

### How It Works

When you ask the agent to place an order without specifying the order type, it automatically uses `LIMIT`:

```text
You: Buy 2 lots of Bank Nifty 52000 PE

Agent: I'll place a LIMIT order (default). What price would you like to set?
```

To use a MARKET order, you must explicitly say so:

```text
You: Buy 2 lots of Bank Nifty 52000 PE at market price
```

The agent will still confirm the order before placing, giving you a final checkpoint even with MARKET orders.

> **note:** The LIMIT default applies to all order varieties — regular orders, After Market Orders (AMO), and bracket/cover orders. Super Orders and Forever Orders also respect this default.

## Lot Size Validation

**F&O orders are validated against exchange-defined lot sizes before submission.**

Every Futures & Options instrument on Indian exchanges has a minimum lot size defined by the exchange. Orders that don't conform to valid lot sizes get rejected by the exchange, wasting time and potentially missing trading opportunities.

### What Gets Validated

The agent checks:

- **Minimum lot size** — Ensures the quantity meets the exchange-defined minimum (e.g., Nifty = 25, Bank Nifty = 15)
- **Lot size multiples** — Ensures the quantity is a valid multiple of the lot size (e.g., 50 for Nifty is valid, 30 is not)
- **Instrument-specific rules** — Different instruments have different lot sizes; the agent uses the current instrument master data

### Example

```text
You: Buy 20 units of Nifty 24500 CE

Agent: ⚠️ Invalid quantity. Nifty options have a lot size of 25.
  The minimum order is 25 units (1 lot). Would you like me to
  place an order for 25 units instead?
```

The agent catches the error before it reaches the exchange, saving you from a rejected order and letting you correct the quantity immediately.

### Supported Segments

Lot size validation applies to all F&O segments:

- **NSE F&O** — Nifty, Bank Nifty, FinNifty, stock options and futures
- **BSE F&O** — Sensex, Bankex options and futures
- **MCX** — Commodity futures and options (Gold, Silver, Crude, etc.)

For equity (cash) segments, there is no lot size constraint — you can buy or sell any quantity of shares.

## How Guardrails Work Together

The three safety mechanisms form a layered defense:

1. **Lot size validation** catches quantity errors early, before the order is even formatted
2. **LIMIT order default** prevents price-related risks by requiring explicit price specification
3. **Order confirmation** gives you a final review of the complete order before it hits the exchange

```text
Your instruction → Lot size check → LIMIT default applied → Full order shown → You confirm → Order placed
```

This layered approach means that even if one guardrail doesn't catch an issue, the next one will. The confirmation step is always the final gate — nothing executes without your explicit approval.

## Overriding Guardrails

These guardrails are designed to protect, not restrict. You can override them when needed:

- **MARKET orders** — Explicitly request "at market" or "market order" in your instruction
- **Confirmation** — Cannot be bypassed. Every order requires confirmation regardless of other settings.

The order confirmation requirement is non-negotiable by design. When real money is at stake, a human-in-the-loop checkpoint is essential for responsible AI-assisted trading.

## Next Steps

- Explore [skill categories](/skills/) to see what your agent can do
- Learn about [Orders](/skills/categories/orders) for detailed order placement capabilities
- Read the [Common Workflows](/skills/categories/common-workflows) for end-to-end trading recipes

---

## Backtesting

Test trading strategies against historical data to evaluate performance before going live.

The Backtesting skill enables your AI agent to test trading strategies against historical market data — evaluating performance, risk metrics, and edge before deploying strategies with real capital.

## Capabilities

- **Strategy simulation** — Run any trading strategy against historical OHLC data
- **Performance metrics** — Calculate returns, Sharpe ratio, max drawdown, win rate, and more
- **Multi-timeframe testing** — Backtest on daily, hourly, or minute-level data
- **Parameter optimization** — Test strategy variations to find optimal parameters
- **Trade log** — Generate detailed entry/exit logs with P&L for each trade

## Metrics Provided

| Metric | Description |
|--------|-------------|
| Total Return | Cumulative P&L over the test period |
| Sharpe Ratio | Risk-adjusted return measure |
| Max Drawdown | Largest peak-to-trough decline |
| Win Rate | Percentage of profitable trades |
| Profit Factor | Gross profit divided by gross loss |
| Average Trade | Mean P&L per trade |

## Use Cases

Ask your agent things like:

- "Backtest a 20/50 EMA crossover strategy on NIFTY for the last year"
- "What's the max drawdown of selling weekly strangles on BANKNIFTY?"
- "Test my RSI-based entry strategy on RELIANCE daily data"
- "Compare performance of iron condors vs straddles on NIFTY over 6 months"

## Notes

Backtesting uses historical data from the Market Data skill. Results are theoretical and do not account for slippage, impact cost, or execution delays. Always validate with paper trading before deploying live.

## Related

- [Market Data](/skills/categories/market-data)
- [Options Analysis](/skills/categories/options-analysis)
- [Common Workflows](/skills/categories/common-workflows)

---

## Common Workflows

End-to-end recipes for frequent trading tasks combining multiple skills.

The Common Workflows skill provides your AI agent with end-to-end recipes for frequently performed trading tasks — combining multiple skills (orders, portfolio, market data) into cohesive workflows.

## Capabilities

- **Multi-step recipes** — Complete workflows that chain multiple API calls in the correct sequence
- **Best practices** — Recommended patterns for common trading operations
- **Error recovery** — Built-in handling for common failure scenarios within workflows
- **Parameterized templates** — Reusable workflow templates that adapt to your specific instruments and quantities

## Example Workflows

### Place an Order with Pre-checks
1. Check available margin (Funds)
2. Validate instrument and lot size (Instruments)
3. Get current market price (Market Data)
4. Place the order with confirmation (Orders)
5. Verify order status (Orders)

### Monitor and Exit a Position
1. Check current positions (Portfolio)
2. Stream live price (Live Feed)
3. When target/stop-loss hit, place exit order (Orders)
4. Confirm execution (Orders)

### Options Strategy Execution
1. Fetch option chain (Option Chain)
2. Analyze Greeks and select strikes (Options Analysis)
3. Calculate margin requirement (Funds)
4. Place multi-leg order (Orders)

## Use Cases

Ask your agent things like:

- "Buy 1 lot of NIFTY 22000 CE with all pre-checks"
- "Set up a stop-loss exit for my RELIANCE position"
- "Execute an iron condor on BANKNIFTY"
- "Show me the full workflow for placing a bracket order"

## Related

- [Orders](/skills/categories/orders)
- [Portfolio](/skills/categories/portfolio)
- [Safety Guardrails](/skills/getting-started/safety-guardrails)

---

## Error Codes

Reference DhanHQ error codes and troubleshooting guidance for common API issues.

The Error Codes skill provides your AI agent with a comprehensive reference of DhanHQ API error codes, their meanings, and troubleshooting steps — enabling it to diagnose and resolve issues without manual lookup.

## Capabilities

- **Error code lookup** — Instantly identify what any DhanHQ error code means
- **Troubleshooting guidance** — Get actionable steps to resolve specific errors
- **Common patterns** — Recognize frequently encountered error scenarios and their fixes
- **Order rejection reasons** — Understand why orders get rejected by the exchange or RMS
- **API-specific errors** — Reference errors specific to each API endpoint

## Error Categories

| Category | Description |
|----------|-------------|
| Authentication | Token expiry, invalid credentials, permission issues |
| Order Validation | Invalid parameters, lot size errors, price band violations |
| Exchange Rejection | RMS limits, circuit breaker, market timing |
| Rate Limiting | API throttling and request limits |
| Data Errors | Invalid instrument IDs, missing data, format issues |

## Use Cases

Ask your agent things like:

- "What does error code DH-901 mean?"
- "Why was my order rejected with 'insufficient margin'?"
- "How do I fix an authentication token expiry error?"
- "What are the common reasons for order rejection on NSE?"

## Related

- [Orders](/skills/categories/orders)
- [Common Workflows](/skills/categories/common-workflows)

---

## Funds

Check available margins, fund balances, and margin requirements through your AI agent.

The Funds skill enables your AI agent to check your available trading margins, fund balances, and margin utilization — helping you understand your buying power before placing orders.

## Capabilities

- **Available margin** — Check total available margin across all segments
- **Fund balances** — View opening balance, realized P&L, and collateral values
- **Margin utilization** — See how much margin is currently blocked by open positions
- **Segment-wise breakdown** — Get margin details split by equity, F&O, currency, and commodity segments
- **Margin calculator** — Estimate margin required for a proposed order before placing it

## Use Cases

Ask your agent things like:

- "How much margin do I have available?"
- "What's my fund balance for F&O trading?"
- "How much margin would I need to buy 1 lot of NIFTY futures?"
- "Show me my margin utilization breakdown"

## Data Points

The skill provides access to available balance, utilized margin, opening balance, payin amount, collateral value, and segment-wise margin allocation — giving your agent complete visibility into your trading capacity.

## Related

- [Orders](/skills/categories/orders)
- [Portfolio](/skills/categories/portfolio)

---

## Instruments

Access the complete instrument universe across all Indian exchanges — NSE, BSE, and MCX.

The Instruments skill gives your AI agent access to the full instrument universe of Indian exchanges, enabling it to look up security IDs, resolve symbols, and understand exchange-specific instrument metadata.

## Capabilities

- **Instrument lookup** — Search instruments by name, symbol, or ISIN across NSE, BSE, and MCX
- **Security ID resolution** — Map trading symbols to DhanHQ security IDs required for order placement
- **Exchange segments** — Query instruments filtered by segment (equity, F&O, currency, commodity)
- **Instrument metadata** — Access lot sizes, tick sizes, expiry dates, and instrument types
- **Bulk download** — Retrieve the complete instrument master file for offline processing

## Exchange Segments

| Segment | Exchange | Examples |
|---------|----------|----------|
| NSE Equity | NSE | RELIANCE, TCS, INFY |
| NSE F&O | NSE | NIFTY options, BANKNIFTY futures |
| BSE Equity | BSE | Listed equities on BSE |
| BSE F&O | BSE | SENSEX options |
| MCX Commodity | MCX | GOLD, SILVER, CRUDE |
| NSE Currency | NSE | USDINR futures and options |

## Use Cases

Ask your agent things like:

- "What's the security ID for RELIANCE on NSE?"
- "Show me all NIFTY weekly expiries available"
- "What's the lot size for BANKNIFTY options?"
- "List all instruments in the MCX commodity segment"

## Related

- [Orders](/skills/categories/orders)
- [Option Chain](/skills/categories/option-chain)
- [Market Data](/skills/categories/market-data)

---

## Live Feed

Stream real-time market quotes and order book updates via WebSocket connections.

The Live Feed skill enables your AI agent to establish WebSocket connections for streaming real-time market data — live quotes, order book depth, and tick-by-tick updates for any instrument on Indian exchanges.

## Capabilities

- **Live quotes** — Stream real-time LTP, bid/ask, and volume for subscribed instruments
- **Market depth** — Access Level 2 order book data with best 5 bid/ask prices and quantities
- **Tick data** — Receive tick-by-tick price updates for high-frequency monitoring
- **Multi-instrument subscription** — Subscribe to multiple instruments simultaneously
- **Connection management** — Handle WebSocket lifecycle (connect, subscribe, unsubscribe, disconnect)

## Feed Modes

| Mode | Data Included |
|------|---------------|
| LTP | Last traded price only |
| Quote | LTP + OHLC + volume + OI |
| Full | Quote + market depth (5 levels) |

## Use Cases

Ask your agent things like:

- "Start streaming live prices for NIFTY and BANKNIFTY"
- "Show me the order book depth for RELIANCE"
- "Subscribe to tick data for my watchlist"
- "What's the current bid-ask spread on HDFC Bank?"

## Technical Notes

Live Feed uses WebSocket connections for low-latency data delivery. The agent manages connection state, handles reconnection on disconnects, and efficiently subscribes/unsubscribes instruments as needed.

## Related

- [Market Data](/skills/categories/market-data)
- [Option Chain](/skills/categories/option-chain)
- [Common Workflows](/skills/categories/common-workflows)

---

## Market Data

Fetch OHLC, historical data, and intraday candles for any instrument on Indian exchanges.

The Market Data skill enables your AI agent to fetch historical and intraday price data for any instrument traded on Indian exchanges — equities, derivatives, currencies, and commodities.

## Capabilities

- **Historical OHLC** — Retrieve daily, weekly, or monthly candlestick data for any date range
- **Intraday candles** — Get 1-minute, 5-minute, 15-minute, 25-minute, and 60-minute intraday bars
- **Last traded price** — Fetch the most recent price for any instrument
- **Daily data** — Open, high, low, close, and volume for the current or past trading sessions

## Supported Timeframes

| Interval | Description |
|----------|-------------|
| 1 minute | Intraday granular data |
| 5 minutes | Short-term intraday |
| 15 minutes | Medium intraday |
| 25 minutes | Extended intraday |
| 60 minutes | Hourly bars |
| Daily | End-of-day OHLCV |

## Use Cases

Ask your agent things like:

- "Get the last 30 days of daily candles for RELIANCE"
- "Show me 5-minute intraday data for NIFTY 50 today"
- "What's the current price of HDFC Bank?"
- "Fetch weekly OHLC for TCS from January to March"

## Related

- [Live Feed](/skills/categories/live-feed)
- [Option Chain](/skills/categories/option-chain)
- [Backtesting](/skills/categories/backtesting)

---

## Option Chain

Retrieve real-time option chain data with Greeks for any underlying on Indian exchanges.

The Option Chain skill provides your AI agent with real-time option chain data including Greeks, open interest, and implied volatility for any underlying instrument on NSE.

## Capabilities

- **Full option chain** — Retrieve calls and puts across all available strike prices for an expiry
- **Greeks** — Access Delta, Gamma, Theta, Vega, and Rho for each contract
- **Open Interest (OI)** — View current OI and OI changes for strike-level analysis
- **Implied Volatility** — Get IV for individual strikes and the overall IV skew
- **Expiry selection** — Query chains for weekly, monthly, or any available expiry date

## Use Cases

Ask your agent things like:

- "Show me the NIFTY option chain for this week's expiry"
- "What's the OI at 22000 CE for NIFTY?"
- "Get Greeks for BANKNIFTY 48000 PE"
- "Which strikes have the highest open interest change today?"

## Data Points

Each option chain entry includes strike price, bid/ask prices, last traded price, volume, open interest, OI change, and computed Greeks — giving your agent everything needed for options analysis and strategy selection.

## Related

- [Options Analysis](/skills/categories/options-analysis)
- [Market Data](/skills/categories/market-data)
- [Instruments](/skills/categories/instruments)

---

## Options Analysis

Strategy building, payoff analysis, and implied volatility calculations for options trading.

The Options Analysis skill enables your AI agent to perform options-specific analysis — strategy construction, payoff calculations, implied volatility analysis, and Greeks-based decision making for F&O trading on Indian exchanges.

## Capabilities

- **Strategy building** — Construct multi-leg options strategies (spreads, straddles, strangles, iron condors, butterflies)
- **Payoff analysis** — Calculate theoretical P&L at various price levels for any strategy
- **IV analysis** — Analyze implied volatility levels, IV rank, and IV percentile
- **Greeks computation** — Calculate and interpret Delta, Gamma, Theta, Vega for position sizing
- **Strike selection** — Recommend optimal strikes based on risk/reward criteria

## Common Strategies

| Strategy | Legs | Market View |
|----------|------|-------------|
| Bull Call Spread | 2 | Moderately bullish |
| Bear Put Spread | 2 | Moderately bearish |
| Iron Condor | 4 | Range-bound / neutral |
| Straddle | 2 | High volatility expected |
| Butterfly | 3 | Low volatility / pinning |

## Use Cases

Ask your agent things like:

- "Build an iron condor on NIFTY with 200-point wings"
- "What's the max profit/loss on a bull call spread at 22000/22200?"
- "Show me the IV percentile for BANKNIFTY options"
- "Which strikes have the best theta decay for weekly expiry?"

## Related

- [Option Chain](/skills/categories/option-chain)
- [Common Workflows](/skills/categories/common-workflows)
- [Backtesting](/skills/categories/backtesting)

---

## Orders

Place, modify, and cancel orders across NSE, BSE, and MCX using AI agent commands.

The Orders skill enables your AI agent to place, modify, and cancel orders across all Indian exchanges — NSE, BSE, and MCX — through natural language commands.

## Capabilities

- **Place orders** — Equity, F&O, currency, and commodity orders with full parameter control (price, quantity, order type, product type, validity)
- **Modify orders** — Update pending orders with new price, quantity, or trigger conditions
- **Cancel orders** — Cancel individual orders or bulk cancel by segment/status
- **Order status** — Check real-time status of any order by ID
- **Order history** — Retrieve the full audit trail of an order's state transitions
- **Super Orders** — Place bracket and cover orders with stop-loss and target legs
- **Forever Orders (GTT)** — Create Good Till Triggered orders that persist across sessions
- **After Market Orders (AMO)** — Queue orders for next trading session execution

## Supported Order Types

| Type | Description |
|------|-------------|
| LIMIT | Execute at specified price or better |
| MARKET | Execute immediately at best available price |
| STOP_LOSS | Trigger at stop price, then execute as limit |
| STOP_LOSS_MARKET | Trigger at stop price, then execute at market |

## Safety Notes

All order operations require explicit user confirmation before execution. The agent defaults to LIMIT orders unless you specifically request MARKET orders. Lot sizes are validated against exchange minimums for F&O instruments.

## Related

- [Safety Guardrails](/skills/getting-started/safety-guardrails)
- [Common Workflows](/skills/categories/common-workflows)

---

## Portfolio

View holdings, positions, and trade history through your AI agent.

The Portfolio skill gives your AI agent access to your complete portfolio data — holdings, positions, and trade history — enabling informed trading decisions through natural language queries.

## Capabilities

- **Holdings** — View your long-term equity holdings with current market value, P&L, and average cost
- **Positions** — Monitor intraday and carry-forward positions across all segments
- **Trade history** — Retrieve executed trades for the current session or historical dates
- **P&L calculations** — Get realized and unrealized profit/loss breakdowns
- **Position conversion** — Convert positions between intraday (MIS) and delivery (CNC/NRML)

## Use Cases

Ask your agent things like:

- "Show me my current holdings"
- "What's my P&L on RELIANCE positions today?"
- "List all trades executed this session"
- "Convert my NIFTY position from MIS to NRML"

## Data Points

The skill provides access to key portfolio metrics including quantity, average price, last traded price, day's P&L, overall P&L, and exchange-specific details for each instrument.

## Related

- [Orders](/skills/categories/orders)
- [Market Data](/skills/categories/market-data)
- [Funds](/skills/categories/funds)

---

## ScanX

Screen and filter stocks using AI-powered scanning tools across Indian markets.

The ScanX skill enables your AI agent to screen and filter stocks across Indian exchanges using powerful scanning criteria and pre-built strategies.

## Capabilities

- **Technical scans** — Screen stocks based on technical indicators (RSI, MACD, moving averages, volume patterns)
- **Fundamental filters** — Filter by market cap, P/E ratio, sector, and other fundamental parameters
- **Pre-built scanners** — Access ready-made scanning strategies for common trading setups
- **Custom criteria** — Define custom scan conditions combining multiple parameters
- **Real-time results** — Get live scan results during market hours

## Use Cases

| Scan Type | Description |
|-----------|-------------|
| Breakout Scanner | Identify stocks breaking above resistance levels |
| Volume Spike | Find stocks with unusual volume activity |
| Momentum | Screen for stocks with strong directional momentum |
| Gap Scanner | Detect opening gaps for intraday opportunities |

## Related

- [Market Data](/skills/categories/market-data)
- [Options Analysis](/skills/categories/options-analysis)
