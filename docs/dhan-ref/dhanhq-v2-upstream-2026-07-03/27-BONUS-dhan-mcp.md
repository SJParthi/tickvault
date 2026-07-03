# DhanHQ API v2 — BONUS: Dhan MCP Server

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/mcp/


---

## What is MCP?

The Model Context Protocol is an open standard that connects AI applications to external tools and data sources through a universal interface.

The **Model Context Protocol (MCP)** is an open standard that defines how AI applications connect to external tools, data sources, and services. It provides a universal interface — so instead of building custom integrations for every AI model and every API, you build one MCP server and it works with any compatible client.

Think of it as a standardized connector between AI and the outside world.

---

## The problem MCP solves

Without MCP, every AI application needs bespoke code to talk to each external service. If you want Claude, ChatGPT, Cursor, and Kiro to all access your trading account, you'd traditionally need four separate integrations.

MCP eliminates this. You build one server, and any MCP-compatible client can connect to it immediately.

---

## How MCP works

MCP uses a client-server architecture:

- **MCP Client** (Host) — The AI application you interact with. Claude Desktop, Cursor, Kiro, VS Code, and many others act as MCP clients.
- **MCP Server** — A service that exposes capabilities (tools, resources, prompts) over the MCP protocol. Dhan MCP is one such server.
- **Protocol** — The standardized communication layer between client and server. Handles tool discovery, invocation, and response formatting.

When you connect a client to a server:

1. The client discovers what tools the server offers
2. When you ask a question, the AI decides which tool to call
3. The client sends the tool call to the server
4. The server executes it and returns the result
5. The AI incorporates the result into its response

---

## Why MCP for trading?

Trading requires real-time data, authenticated access, and careful execution. MCP is well-suited because:

- **Scoped authentication** — Your session stays on the server. No tokens in config files.
- **Tool-level granularity** — Each capability (place order, check margin, fetch quotes) is a discrete tool the AI can invoke.
- **Confirmation flows** — The AI shows you order details before submitting. You stay in control.
- **Client-agnostic** — Switch between Claude, Cursor, or any other client without reconfiguring your trading access.

---

## The standard

MCP was originally developed by [Anthropic](https://www.anthropic.com) and released as an open standard. It has since been adopted by a broad ecosystem of AI tools and agent platforms.

The full specification and documentation are available at [modelcontextprotocol.io](https://modelcontextprotocol.io).

---

## Next

- [Architecture](/mcp/overview/architecture) — How Dhan MCP implements the protocol
- [Installation](/mcp/getting-started/installation) — Connect your first client

---

## Dhan MCP

Connect your live Dhan trading account to any MCP-compatible AI client — trade, manage portfolio, fetch market data, set alerts, and calculate margins through natural language.

**A Model Context Protocol server for live trading on Indian exchanges.**

Dhan MCP connects your authenticated Dhan trading account to any MCP-compatible AI client. Trade equities and F&O, manage your portfolio, fetch market data, set alerts, and calculate margins — entirely through natural language.

Built on the open [Model Context Protocol](https://modelcontextprotocol.io) standard, Dhan MCP works with Claude Desktop, Cursor, OpenCode, Kiro, VS Code, and any other client that speaks MCP.

---

## How it works


**Flow:** You → MCP Client → mcp.dhan.co → DEXT → Exchange

1. **You** — Give a natural language instruction (e.g. "Show my holdings", "Place a buy order")
2. **MCP Client** — Claude, Cursor, Kiro, or another MCP client interprets intent and selects a tool
3. **mcp.dhan.co** — Dhan MCP Server converts the tool call into the correct API action
4. **DEXT** — Dhan internal infrastructure handles routing, validation, and service communication
5. **Exchange** — Request is routed to NSE, BSE, or MCX

---

## Connect your client


Select your AI client and follow the instructions below.


### Claude Web

1. Go to [Claude.ai](https://claude.ai) and open **Settings** → **Connectors** → **Customize**
2. Tap the **+** button beside the search bar and choose **Add Custom Connector**
3. Set the name to **Dhan** and enter the MCP URL:

```
https://mcp.dhan.co/mcp
```

4. Hit **Connect** and finish the Dhan authentication flow
5. Dhan tools will now be available in your Claude conversations

This connector also syncs to Claude Desktop and Claude CLI. Restart them if tools don't appear right away.

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Claude Code

1. Install Claude Code CLI if you haven't already — see the [official docs](https://docs.anthropic.com/en/docs/claude-code)
2. Run the following in your terminal:

```bash
claude mcp add --transport http dhan https://mcp.dhan.co/mcp
```

3. Launch Claude Code by typing `claude` in your terminal
4. Enter `/mcp` and pick **dhan** from the list
5. Choose **Authenticate** and hit Enter — your browser will open for Dhan login
6. Complete sign-in with your Dhan credentials. You'll land on a confirmation page
7. Back in the terminal, run `/mcp` again to confirm the status is connected

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### ChatGPT

A native Dhan integration for ChatGPT is on the way. For now, connect manually via developer mode.

1. In ChatGPT, tap your profile icon (bottom left), then head to **Settings** → **Apps**
2. Open **Advanced Settings** and turn on **Developer Mode**
3. With that enabled, tap **Create App** at the top of the popup (beside the back button)
4. Enter the name as **Dhan**, set authentication to **OAuth**, and add the MCP URL:

```
https://mcp.dhan.co/mcp
```

5. Confirm by clicking **Create** after acknowledging *I understand*
6. When prompted to add Dhan MCP, toggle on **Referencing Memories** for richer responses, then tap **Sign In**
7. Authenticate on the Dhan login page — you'll be redirected back to ChatGPT afterward
8. Open a new chat and try the verification prompt below

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Codex

1. Install the Codex CLI if needed — see the [official repo](https://github.com/openai/codex)
2. Run this command in your terminal:

```bash
codex mcp add dhan --url https://mcp.dhan.co/mcp
```

3. Your browser will open for Dhan authentication — sign in and setup completes automatically
4. Launch Codex by typing `codex` in your terminal
5. Enter `/mcp` to confirm the Dhan server shows as connected

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Cursor

1. In Cursor, open **Settings** (beside your profile in the bottom left)
2. Navigate to **Tools & MCPs** in the left sidebar
3. Click **New MCP Server** under Home MCP Servers — a config file will open in the editor
4. Paste the following and save (Cmd+S / Ctrl+S):

```json
{
  "mcpServers": {
    "dhan": {
      "url": "https://mcp.dhan.co/mcp"
    }
  }
}
```

5. Dhan will appear in your MCP servers list. Click **Connect** to launch the auth page
6. Log in with your Dhan credentials — you'll be taken back to Cursor with a connected status
7. Return to the chat and begin a new conversation

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### OpenCode

Download [OpenCode](https://opencode.ai/download) if not already installed. Install the terminal version first — once connected, you can also use it with OpenCode Desktop.

1. In your terminal, run `opencode mcp add` and follow the prompts:
   - **Enter MCP server name:** `dhan`
   - **Select MCP server type:** Remote
   - **Enter MCP server URL:** `https://mcp.dhan.co/mcp`
   - **Does this server require OAuth authentication:** Yes
   - **Do you have a pre-registered client ID:** No

2. Run the following command to authenticate:

```bash
opencode mcp auth dhan
```

3. This will open the Dhan OAuth page in your browser — sign in to complete authentication

4. Open OpenCode by typing `opencode` in your terminal and type `/mcp` to verify the Dhan server is connected

This will also add the MCP server to your OpenCode Desktop application. Restart OpenCode Desktop if the server is not immediately visible.

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Custom

For any client that supports MCP, use these connection details:

- **Name:** Dhan
- **Authentication:** OAuth
- **MCP URL:**

```
https://mcp.dhan.co/mcp
```

Authentication uses OAuth. When your client requests login, you'll be directed to Dhan to sign in with your account.

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

---

## What you can do

| Capability | Description |
|-----------|-------------|
| **Portfolio** | Check funds, holdings, positions, and today's trades |
| **Orders** | Place, modify, and cancel orders — including Super Orders |
| **Market data** | Live prices, quotes with depth, option chains |
| **Historical data** | OHLCV candles across multiple timeframes |
| **Margin** | Pre-order margin checks for single or basket orders |
| **Alerts** | Price and indicator-triggered alerts with linked orders |
| **Search** | Resolve company names and tickers to Dhan security IDs |

---

## Next steps

- [What is MCP?](/mcp/overview/what-is-mcp) — Understand the protocol standard
- [Architecture](/mcp/overview/architecture) — How Dhan MCP fits into the ecosystem
- [Installation](/mcp/getting-started/installation) — Client-specific setup details
- [Tools](/mcp/tools/portfolio) — Full tool documentation
- [REST APIs](/api/v2/) — HTTP API reference
- [Agent Skills](/skills/) — AI-agent skill pack for coding agents
- [Developer Portal](https://developer.dhanhq.co) — Manage your API access

---

## Architecture

How Dhan MCP connects your AI client to your live trading account through the Model Context Protocol.

# Dhan MCP Architecture Flow

This diagram shows how a developer can interact with Dhan's trading infrastructure using natural language through an MCP-enabled client.


**Flow:** You → MCP Client → mcp.dhan.co → DEXT → Exchange

1. **You** — Give a natural language instruction (e.g. "Show my holdings", "Place a buy order")
2. **MCP Client** — Claude, Cursor, Kiro, or another MCP client interprets intent and selects a tool
3. **mcp.dhan.co** — Dhan MCP Server converts the tool call into the correct API action
4. **DEXT** — Dhan internal infrastructure handles routing, validation, and service communication
5. **Exchange** — Request is routed to NSE, BSE, or MCX

---

## 1. You

The developer gives a natural language instruction, such as:

- "Show my holdings"
- "Place a buy order"
- "Get NIFTY option chain"

This starts the workflow.

---

## 2. MCP Client

The instruction is sent to an MCP-compatible client such as Claude, Cursor, or Kiro.

The client understands the user's intent and decides which tool needs to be called.

---

## 3. Tool Call to mcp.dhan.co

The MCP Client sends a structured tool call to `mcp.dhan.co`.

This acts as the Dhan MCP Server.

---

## 4. Dhan MCP Server

The MCP Server exposes Dhan's trading tools and capabilities through the MCP protocol.

It converts the client's request into the correct backend/API action.

---

## 5. DEXT

The request is passed to Dhan's internal infrastructure, shown as DEXT.

This layer handles routing, validation, and communication with trading services.

---

## 6. Exchange

Finally, the request is routed to the relevant exchange:

- NSE
- BSE
- MCX

The exchange processes the trading-related request.

---

## Simple Flow

**You → MCP Client → mcp.dhan.co → DEXT → Exchange**

---

## Requirements

What you need before connecting to Dhan MCP.

| Requirement | Needed for |
|-------------|-----------|
| Dhan trading account | Everything |
| Node.js v18+ | Running the MCP transport |
| MCP-compatible client | Interacting with the server |
| Data API subscription (optional) | Live quotes, option chains |

## Node.js

```bash
# macOS (Homebrew)
brew install node

# Windows (winget)
winget install OpenJS.NodeJS

# Verify
node --version
npx --version
```

## Compatible clients

- [Claude Desktop](https://claude.ai/download)
- [Cursor](https://cursor.com)
- [Kiro](https://kiro.dev)
- VS Code with MCP extensions
- Any custom application built on an MCP SDK

---

## Installation

Set up Dhan MCP with your preferred AI client.

Select your AI client and follow the instructions below.


### Claude Web

1. Go to [Claude.ai](https://claude.ai) and open **Settings** → **Connectors** → **Customize**
2. Tap the **+** button beside the search bar and choose **Add Custom Connector**
3. Set the name to **Dhan** and enter the MCP URL:

```
https://mcp.dhan.co/mcp
```

4. Hit **Connect** and finish the Dhan authentication flow
5. Dhan tools will now be available in your Claude conversations

This connector also syncs to Claude Desktop and Claude CLI. Restart them if tools don't appear right away.

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Claude Code

1. Install Claude Code CLI if you haven't already — see the [official docs](https://docs.anthropic.com/en/docs/claude-code)
2. Run the following in your terminal:

```bash
claude mcp add --transport http dhan https://mcp.dhan.co/mcp
```

3. Launch Claude Code by typing `claude` in your terminal
4. Enter `/mcp` and pick **dhan** from the list
5. Choose **Authenticate** and hit Enter — your browser will open for Dhan login
6. Complete sign-in with your Dhan credentials. You'll land on a confirmation page
7. Back in the terminal, run `/mcp` again to confirm the status is connected

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### ChatGPT

A native Dhan integration for ChatGPT is on the way. For now, connect manually via developer mode.

1. In ChatGPT, tap your profile icon (bottom left), then head to **Settings** → **Apps**
2. Open **Advanced Settings** and turn on **Developer Mode**
3. With that enabled, tap **Create App** at the top of the popup (beside the back button)
4. Enter the name as **Dhan**, set authentication to **OAuth**, and add the MCP URL:

```
https://mcp.dhan.co/mcp
```

5. Confirm by clicking **Create** after acknowledging *I understand*
6. When prompted to add Dhan MCP, toggle on **Referencing Memories** for richer responses, then tap **Sign In**
7. Authenticate on the Dhan login page — you'll be redirected back to ChatGPT afterward
8. Open a new chat and try the verification prompt below

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Codex

1. Install the Codex CLI if needed — see the [official repo](https://github.com/openai/codex)
2. Run this command in your terminal:

```bash
codex mcp add dhan --url https://mcp.dhan.co/mcp
```

3. Your browser will open for Dhan authentication — sign in and setup completes automatically
4. Launch Codex by typing `codex` in your terminal
5. Enter `/mcp` to confirm the Dhan server shows as connected

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Cursor

1. In Cursor, open **Settings** (beside your profile in the bottom left)
2. Navigate to **Tools & MCPs** in the left sidebar
3. Click **New MCP Server** under Home MCP Servers — a config file will open in the editor
4. Paste the following and save (Cmd+S / Ctrl+S):

```json
{
  "mcpServers": {
    "dhan": {
      "url": "https://mcp.dhan.co/mcp"
    }
  }
}
```

5. Dhan will appear in your MCP servers list. Click **Connect** to launch the auth page
6. Log in with your Dhan credentials — you'll be taken back to Cursor with a connected status
7. Return to the chat and begin a new conversation

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### OpenCode

Download [OpenCode](https://opencode.ai/download) if not already installed. Install the terminal version first — once connected, you can also use it with OpenCode Desktop.

1. In your terminal, run `opencode mcp add` and follow the prompts:
   - **Enter MCP server name:** `dhan`
   - **Select MCP server type:** Remote
   - **Enter MCP server URL:** `https://mcp.dhan.co/mcp`
   - **Does this server require OAuth authentication:** Yes
   - **Do you have a pre-registered client ID:** No

2. Run the following command to authenticate:

```bash
opencode mcp auth dhan
```

3. This will open the Dhan OAuth page in your browser — sign in to complete authentication

4. Open OpenCode by typing `opencode` in your terminal and type `/mcp` to verify the Dhan server is connected

This will also add the MCP server to your OpenCode Desktop application. Restart OpenCode Desktop if the server is not immediately visible.

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP

### Custom

For any client that supports MCP, use these connection details:

- **Name:** Dhan
- **Authentication:** OAuth
- **MCP URL:**

```
https://mcp.dhan.co/mcp
```

Authentication uses OAuth. When your client requests login, you'll be directed to Dhan to sign in with your account.

Once set up, verify the connection with this prompt:

> Check my holdings with Dhan MCP


## Prerequisites

- An active Dhan account with trading access
- Data API subscription (optional, needed only for live market quotes and option chains)

## Verify the connection

After setup, ask your AI client:

> "What tools do you have from Dhan?"

It should list the available tools: portfolio, orders, trading, market data, historical data, margin, alerts, and search.

---

## First Query

Run your first interaction with Dhan MCP.

Once installed, try asking your AI client:

> "What are my available funds?"

The AI calls `portfolio_agent_tool`, fetches your live account data, and presents it:

```
Your available funds:
- Cash available: ₹1,24,500
- Margin used: ₹45,000
- Margin available: ₹79,500
```

No code. No manual API calls. Just a question and a live answer.

---

## Try more

- *"Show me my holdings"*
- *"What's the current price of RELIANCE?"*
- *"How much margin do I need to buy 10 INFY intraday?"*
- *"Place a limit buy for 5 HDFCBANK at ₹1650, delivery"* (places a real order with confirmation)

---

## Alerts

Create price or indicator-triggered alerts with linked orders.

**Tool:** `alerts_agent_tool`

> **Equities Only:** Supports **equities and indices only**. F&O, commodity, and currency are not supported.

Create price or indicator-triggered alerts with a linked order that fires automatically when the condition is met.

---

## Example prompts

- *"Create an alert: if RELIANCE crosses ₹1400 on the 1-min chart, place a LIMIT BUY at ₹1405 for 5 shares intraday."*
- *"Show all my active alerts."*
- *"Delete alert 789..."*

---

## Supported conditions

**Condition types:** price vs a fixed value, technical indicator vs a fixed value, one indicator vs another, indicator vs previous close.

**Operators:** crossing up, crossing down, greater than, less than, equal.

**Timeframes:** day, 1-min, 5-min, 15-min.

---

## Margin

Check required margin before placing any order — single or basket.

**Tool:** `margin_agent_tool`

Check required margin before placing any order — single or basket.

---

## Example prompts

- *"What margin do I need to buy 10 RELIANCE at ₹1350 intraday?"*
- *"Calculate basket margin for buying 5 RELIANCE and selling 2 POWERGRID simultaneously."*

---

## Market Data

Fetch live prices, quotes, option chains, and historical OHLCV candles.

Access real-time and historical market data for equities, F&O, and indices.

---

## Live Market Data

**Tool:** `market_data_agent_tool`

> **Data API Required:** Requires an active **Dhan Data API subscription**. Without it, all actions return Unauthorized.

Fetch live prices, full quotes with order book depth, and option chains.

**Example prompts:**
- *"What's the current LTP for RELIANCE and INFY?"*
- *"Get the full quote with bid/ask depth for HDFCBANK."*
- *"Show me the option chain for NIFTY expiring 26 June 2026."*
- *"What expiry dates are available for BANKNIFTY?"*

---

## Historical Data

**Tool:** `historical_data_agent_tool`

Fetch OHLCV candles across multiple timeframes.

**Example prompts:**
- *"Show 5-minute candles for RELIANCE on 21 May 2026."*
- *"Get daily OHLCV for POWERGRID from 1 Jan to 22 May 2026."*
- *"Fetch 1-hour candles for UCOBANK for the last 30 days."*

**Supported intraday intervals:** `1`, `5`, `15`, `30`, `60` minutes.

---

## Orders

View, place, modify, and cancel orders — including Super Orders.

Manage the full order lifecycle — view your order book, check trade fills, and place or modify live orders.

---

## Order Book

**Tool:** `orderbook_agent_tool`

View and track orders — pending, executed, cancelled, or rejected.

**Example prompts:**
- *"Show all my orders for today including rejected ones."*
- *"Get full details of order 221260522781103 including the rejection reason."*
- *"Show my super order book with all legs."*

---

## Trade Book

**Tool:** `tradebook_agent_tool`

View all fills executed today.

**Example prompts:**
- *"Show all trades executed today."*
- *"Show me all fills for order 123..."*

---

## Trading

**Tool:** `trading_agent_tool`

> **Live Orders:** These place **real orders** on your live Dhan account. The assistant always shows you the full order details before submitting and runs a pre-trade margin check by default.

Place, modify, and cancel orders — including Super Orders.

**Example prompts:**
- *"Place a limit BUY for 10 INFY at ₹1800, intraday, DAY."*
- *"Modify order 123... to ₹1810."*
- *"Cancel order 123..."*
- *"Place a super order: buy RELIANCE at ₹1350, target ₹1400, stop-loss ₹1310, trailing jump ₹5, intraday."*
- *"Move the target leg of super order 456... to ₹1420."*
- *"Cancel the stop-loss leg of super order 456..."*

---

## Portfolio

Check your account snapshot — funds, holdings, positions, and today's trades.

**Tool:** `portfolio_agent_tool`

Check your account snapshot at any time — funds, holdings, positions, and executed trades.

---

## Example prompts

- *"What are my available funds and margin utilization?"*
- *"Show all my holdings with invested value."*
- *"What are my open positions and their P&L?"*
- *"Did I execute any trades today?"*

---

## Search

Resolve company names, tickers, and indices to Dhan security IDs.

**Tool:** `search_agent_tool`

Resolve any company name, ticker, or index to its Dhan security ID. Always use the security ID when placing orders.

---

## Example prompts

- *"What is the security ID for HDFCBANK on NSE?"*
- *"Find all RELIANCE derivatives expiring in May 2026."*

---

## Tips

Pass the exchange (`NSE`, `BSE`, or `MCX`) to narrow results when a name matches multiple instruments.
