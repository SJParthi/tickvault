# DhanHQ API v2 — BONUS: Dhan Cloud (Strategy Hosting)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/cloud/


---

## Dhan Cloud

Deploy and run algorithmic trading strategies on Dhan's managed cloud infrastructure.

**Managed infrastructure for algorithmic trading strategies.**

Dhan Cloud lets you deploy, run, schedule, and scale Python trading strategies on low-latency infrastructure co-located with Indian exchanges — without managing servers or worrying about uptime during market hours.

---

## Why Dhan Cloud

- **Zero infrastructure** — No servers to provision or maintain
- **Co-located execution** — Low-latency connectivity to NSE, BSE, MCX
- **Python native** — Write strategies using the DhanHQ SDK you already know
- **Secure by design** — Code scanning, sandboxed execution, credential isolation
- **Pay per use** — Credit-based billing tied to compute tiers

---

## How it works

1. Write your strategy in Python using the DhanHQ SDK
2. Set credentials and config via secure Variables
3. Declare dependencies (packages installed fresh each deploy)
4. Choose a compute tier and deploy
5. Monitor via live logs

---

## Next steps

- [Getting Started](/cloud/getting-started) — Connect your account and deploy your first strategy
- [Execution Environment](/cloud/core-concepts/execution-environment) — What runs inside Dhan Cloud
- [Compute Tiers](/cloud/core-concepts/compute-tiers) — Available specs and pricing
- [Variables](/cloud/core-concepts/variables) — Securely pass credentials to your code
- [Dependencies](/cloud/core-concepts/dependencies) — Managing Python packages
- [Writing Strategies](/cloud/writing-strategies/) — Patterns and SDK usage
- [Security](/cloud/security/code-scanner) — Code scanning and allowed operations

---

## Getting Started

Connect your Dhan account and deploy your first Python strategy on Dhan Cloud.

## Connect Your Dhan Account

Before you can deploy strategies, you need to connect your Dhan trading account via the Developer Portal. This is a one-time step required for regulatory compliance and links your Dhan ledger balance for billing.

> **note:** Connecting your account does not authorize Dhan Cloud to place orders on your behalf. Orders can only be placed by your strategy code, using the API credentials you configure in Variables. Without those credentials in your code, no orders will be placed.

### Step 1 — Sign in to the Developer Portal

Go to [developer.dhanhq.co](https://developer.dhanhq.co) and sign in or create an account.

### Step 2 — Connect your Dhan account

Follow the steps in the portal to connect your Dhan trading account. Once connected, your Dhan ledger balance can be used to purchase cloud credits.

### Credentials you will need

To place orders and fetch market data, your strategy needs two credentials from Dhan:

| Credential | Where to find it |
|---|---|
| Client ID | Dhan app, under Profile |
| Access Token | [developer.dhanhq.co](https://developer.dhanhq.co), under Access Token |

You will add these to your strategy using Variables, not by writing them directly in code. See [Variables](/cloud/core-concepts/variables) for how this works.

---

## Deploy Your First Strategy

This walkthrough creates and deploys a minimal strategy that fetches a live stock quote and prints it to the logs.

### Step 1 — Create a new strategy

In the Dhan Cloud dashboard, click **New Strategy**. Give it a name.

### Step 2 — Write the strategy

Paste the following into the editor. This strategy fetches the live price of Infosys (security ID `1333` on NSE) and prints it once a minute.

```python
from dhanhq import DhanContext, dhanhq
import time

client_id    = "{{CLIENT_ID}}"
access_token = "{{ACCESS_TOKEN}}"

dhan = dhanhq(DhanContext(client_id, access_token))

while True:
    quote = dhan.ticker_data(securities={"NSE_EQ": [1333]})
    print(f"Infosys LTP: {quote}")
    time.sleep(60)
```

### Step 3 — Set your credentials in Variables

Go to the **Variables** tab and add:

- `CLIENT_ID` with your Dhan client ID
- `ACCESS_TOKEN` with your access token from [developer.dhanhq.co](https://developer.dhanhq.co)

The `{{CLIENT_ID}}` and `{{ACCESS_TOKEN}}` placeholders in your code are replaced with these values at runtime.

### Step 4 — Add the SDK to Dependencies

Go to the **Dependencies** tab and add:

```
dhanhq
```

Dhan Cloud installs packages fresh on each deployment. Even `dhanhq` must be declared here. See [Dependencies](/cloud/core-concepts/dependencies) for more.

### Step 5 — Select a compute tier

In the Settings section, select a compute tier. See [Compute Tiers](/cloud/core-concepts/compute-tiers) for available specs.

### Step 6 — Save and deploy

Click **Save**. You will be asked whether to save as a new version or overwrite the current one. For a first strategy, choose **Save as new version**.

Click **Deploy**. Dhan Cloud runs a code scan first. If the scan passes, the strategy is queued for execution. You can watch live output in the **Logs** tab.

> **tip:** If your strategy does not appear to be running, check the Logs tab first. Most issues (missing packages, wrong credentials) show up there immediately.

See [Versions](/cloud/core-concepts/versions) for how to manage version history and roll back changes.

---

## Testing with Sandbox

Dhan provides a sandbox environment where you can test order placement without using real funds. To use it, get sandbox credentials from [developer.dhanhq.co](https://developer.dhanhq.co) and set your strategy to use the sandbox API endpoint:

```python
# Use separate Variables for sandbox credentials
client_id    = "{{SANDBOX_CLIENT_ID}}"
access_token = "{{SANDBOX_ACCESS_TOKEN}}"

# Sandbox base URL: https://sandbox.dhan.co/v2/
```

The sandbox behaves like the live API but does not execute real orders.

---

## Execution Environment

Python version, runtime restrictions, persistent storage, and execution model for Dhan Cloud strategies.

Dhan Cloud strategies run in an isolated Python container. Each deployment gets a fresh environment. Understanding this helps you avoid common issues.

## Python Version

Python **3.11**. The runtime version is managed by Dhan Cloud and cannot be changed.

## Restricted Operations

The following operations are blocked. If your code uses any of them, the code scanner will prevent deployment.

| Operation | Why it is blocked | What to use instead |
|---|---|---|
| `os.getenv()` | Environment variables are not available in the container | Use `{{VAR_NAME}}` variables |
| `os.path` and other filesystem checks | Container filesystem is not accessible | Use in-memory state |
| `subprocess`, `os.system()`, `os.popen()` | Shell access is not permitted | Not supported; remove these calls |
| Writing files (`open(..., 'w')`) | No writable filesystem | Use `print()` for logging |
| Excel packages (`openpyxl`, `xlwt`, `xlrd`) | Flagged by the security scanner | Use pandas DataFrames in memory |

> **warning:** `os.getenv()` and `os.path` are hard blocked. Code using these will fail the scanner and cannot be deployed. Use the [Variables](/cloud/core-concepts/variables) system instead.

## Persistent Storage

There is no persistent file storage between runs. If your strategy exits and restarts, any local state is lost.

If you need to persist state across runs, options include:

- Writing to an external database via HTTP API (e.g. Firebase, Supabase, or any REST-accessible store)
- Logging events to an external service

## Execution Model

Strategies run as a continuous Python process. Use a `while True` loop with `time.sleep()` to control how frequently your logic executes.

```python
import time

while True:
    run_strategy()
    time.sleep(60)  # runs once per minute
```

> **warning:** Always include `time.sleep()` inside your loop. A loop without a sleep will consume CPU at maximum rate and may be terminated.

---

## Compute Tiers

Dhan Cloud compute tier specs and per-strategy isolation model.

Each strategy you deploy runs on its own compute instance. You select the tier for each strategy in the **Settings** section of the strategy editor.

## Current Tier

| Spec | Value |
|---|---|
| CPU | 1 vCPU |
| Memory | 3 GB RAM |
| Strategies running in parallel | No limit. Each strategy gets its own instance. |
| Log retention | Unlimited |

> **note:** Additional tiers are planned. Check the Settings section in the dashboard for currently available options and pricing.

## Per-Strategy Isolation

Every strategy runs on a dedicated instance. One strategy cannot affect the performance or resources of another.

---

## Credits

Dhan Cloud credit-based billing, free credits on signup, topping up from Dhan ledger, and low-balance behavior.

Dhan Cloud uses a credit-based billing model. Running strategies consume credits over time. When credits run out, strategies stop.

## Free Credits on Signup

Every new Dhan Cloud account starts with **100 free compute credits**, applied automatically when you sign up. No payment is required to get started.

## Adding Credits

Credits are purchased using your Dhan ledger balance.

1. Go to **Settings** in the Dhan Cloud dashboard
2. Select a top-up amount
3. The amount is deducted from your Dhan ledger and added to your Cloud credit balance

> **note:** Topping up requires a connected Dhan account. See [Getting Started](/cloud/getting-started) if you have not connected one yet.

## Low Balance Warning

When your balance drops below **50 credits**, a warning banner appears in the dashboard. Your strategies continue running, but you should add credits soon.

> **warning:** When your balance reaches 0, all running strategies stop. To avoid interruption, top up before running out.

## Credit Consumption

How quickly credits are consumed depends on the compute tier and how long your strategies run. You can check your current balance and usage history in **Settings, Credits**.

---

## Dependencies

How to declare and manage Python package dependencies for Dhan Cloud strategies.

Dhan Cloud installs Python packages fresh on each deployment based on what you declare in the **Dependencies** tab. There is no pre-installed package list. Every package your strategy needs, including `dhanhq`, must be declared.

## Declaring Dependencies

In the **Dependencies** tab, list one package per line:

```
dhanhq==2.0.1
scipy==1.11.0
ta-lib==0.4.28
```
> **tip:** It is necessary to specify the package version every time a dependency is added to the dependencies section.

## How Packages Are Evaluated

Dhan Cloud installs packages from public PyPI. Before installing, each package is checked against the **Python Packaging Advisory Database** and the **OSV (Open Source Vulnerabilities)** database. Packages with known security advisories are rejected. All other packages install normally.

> **warning:** If a package fails to install, your strategy will not start and you will see an error in the Logs. Double-check the package name on PyPI and consider pinning an older version if the latest has a known issue.

---

## Variables

Inject credentials and config into strategies using Dhan Cloud's Variables system, global and strategy-scoped.

let you inject credentials and configuration into strategies without writing sensitive values directly in code. When your strategy runs, Dhan Cloud substitutes the placeholders with the actual values. The values are never visible in logs or stored in your strategy code.

There are two scopes:

## Global Variables

Global variables are defined once and are available to every strategy in your account. Use them for credentials or config that multiple strategies share.

| Use for | Examples |
|---|---|
| Shared API credentials | `CLIENT_ID`, `ACCESS_TOKEN` |
| Common configuration | `RISK_PER_TRADE`, `MAX_POSITION_SIZE` |

Set in: Dashboard, **Settings, Global Variables**

## Strategy Variables

Strategy variables are scoped to a single strategy. If a strategy variable and a global variable share the same name, the strategy variable takes precedence.

Set in: Strategy editor, **Variables tab**

## Using Variables in Code

Reference a variable in your code using double-brace syntax:

```python
client_id    = "{{CLIENT_ID}}"
access_token = "{{ACCESS_TOKEN}}"
```

At runtime, `{{CLIENT_ID}}` is replaced with the actual value you configured.

> **warning:** Do not use `os.getenv()` to read credentials. Environment variables are not available in Dhan Cloud containers. Use the `{{VAR_NAME}}` syntax instead.

> **tip:** Variable names are case-sensitive. `{{CLIENT_ID}}` and `{{client_id}}` are treated as different variables.

---

## Versions

How strategy versioning works in Dhan Cloud, including saving, restoring, and the relationship between editor and deployed versions.

Every time you save a strategy, Dhan Cloud asks how you want to save it:

- **Save as new version** creates a new entry in the version history. The previous version is preserved and can be restored later.
- **Overwrite current version** replaces the current version. The previous state is not retained.

## Version History

Saved versions appear under the **Versions** tab with a timestamp. You can view or restore any version from this list.

> **tip:** Use "Save as new version" before making significant changes. It gives you a checkpoint to return to if something breaks.

## A Few Things to Know

- Version history is per-strategy. Versions are not shared between strategies.
- Restoring a version replaces the current editor content, including code and dependencies.
- The version in the editor and the currently deployed version can be different. Clicking Deploy always uses whatever is currently in the editor.

---

## No Static IP Required

Strategies on Dhan Cloud run on Dhan's own infrastructure and connect to Dhan APIs internally. You do not need to whitelist an IP address or set up firewall rules. Access token credentials are the only thing needed to authenticate.

---

## Allowed Operations

What Python operations are allowed and blocked in Dhan Cloud's isolated container environment.

# Allowed and Restricted Operations

Dhan Cloud strategies run in an isolated container. The following tables describe what is and is not permitted.

## Allowed

| Operation | Notes |
|---|---|
| `requests.get()` / `requests.post()` | Outbound HTTP is permitted to any host |
| `print()` | Output appears in real time in the Logs tab |
| `time.sleep()` | Required inside loops to control execution cadence |
| `pandas`, `numpy`, and most PyPI packages | Declare in Dependencies |
| `dhanhq` | Official Dhan SDK. Declare in Dependencies. |

## Blocked

| Operation | Notes |
|---|---|
| `subprocess`, `os.system()`, `os.popen()` | Shell execution is not supported |
| `os.getenv()` | Environment variables are not available. Use the Variables system instead. |
| `os.path` and filesystem operations | The container filesystem is not accessible |
| Writing files (`open(..., 'w')`) | No writable filesystem. Use `print()` for logging. |
| Excel packages (`openpyxl`, `xlwt`, `xlrd`) | Flagged by the security scanner. Use pandas DataFrames. |
| Binding to network ports | Container networking is isolated |

## Outbound HTTP

There is no allowlist for outbound HTTP. Your strategy can make requests to any public host, including:

- `api.dhan.co` for Dhan trading and market data APIs
- `nseindia.com` or `bseindia.com` for exchange data
- Any other public API your strategy needs

---

## Code Scanner

How Dhan Cloud's AI code scanner works, what it checks, risk levels, common flags and how to fix them.

Every deployment goes through an automated security scan before your strategy starts. This is mandatory. If the scan flags anything, deployment is blocked until you resolve the issues.

## What the Scanner Checks

The scanner uses AI to analyze your code for patterns that could pose a security or stability risk in a shared cloud environment:

| Category | What it looks for |
|---|---|
| Credential exposure | Hardcoded tokens, passwords, or client codes |
| System access | Shell execution, filesystem reads and writes |
| Infinite resource consumption | Loops without throttling, uncontrolled memory use |
| Dependency risk | Packages that wrap or obscure dangerous operations |

## Risk Levels

| Level | What it means | Effect |
|---|---|---|
| `LOW` | Minor concern | Deployment blocked |
| `MEDIUM` | Security concern | Deployment blocked |
| `HIGH` | Security violation | Deployment blocked |

Every flag blocks deployment regardless of severity. There is no way to acknowledge a flag and proceed anyway. All issues must be resolved before you can deploy.

The scanner is AI-powered, which means identical code can occasionally produce slightly different results between scans. If you receive an unexpected flag on code that should be clean, re-save and try again once before investigating further.

## Common Flags and How to Fix Them

**Flag: Hardcoded credentials**
```
The code contains sensitive information placeholders like {CLIENT_CODE}, {TOKEN}
```
- **Risk level:** MEDIUM
- **Fix:** Use double-brace variable syntax. A single brace like `{CLIENT_CODE}` is treated as a literal string, not a variable reference.
```python
# Wrong
client_code = "{CLIENT_CODE}"

# Correct
client_code = "{{CLIENT_CODE}}"
```

---

**Flag: subprocess or shell execution**
```
The code uses subprocess execution
```
- **Risk level:** HIGH
- **Fix:** Remove all `subprocess`, `os.system()`, and `os.popen()` calls. Shell execution is not supported in Dhan Cloud.

---

**Flag: os.getenv() usage**
```
The code uses os.getenv() to read environment variables
```
- **Risk level:** MEDIUM / HIGH
- **Fix:** Replace with variable syntax. Environment variables are not available inside the container.
```python
# Remove this
import os
token = os.getenv("ACCESS_TOKEN")

# Use this instead
token = "{{ACCESS_TOKEN}}"
```

---

**Flag: Infinite loop without throttling**
```
The code runs in an infinite loop which could lead to resource exhaustion
```
- **Risk level:** HIGH
- **Fix:** Add `time.sleep()` inside the loop.
```python
while True:
    run_strategy()
    time.sleep(60)
```

---

**Flag: External HTTP requests**
```
The code makes HTTP requests to external APIs
```
- **Risk level:** MEDIUM (informational)
- **Fix:** No action needed if you are fetching public market data. This flag is raised for awareness. Outbound HTTP to any host is permitted.

---

**Flag: Tradehull library usage**
```
The code uses the Tradehull library which appears to be a wrapper around trading APIs
```
- **Risk level:** LOW (informational)
- **Fix:** No action needed. Tradehull is explicitly allowed on Dhan Cloud.

## If Your Strategy Is Blocked

1. Read the issue list shown in the scan result
2. Fix each flagged item using the guidance above
3. Re-save and re-deploy. The scanner runs on every deployment.

> **tip:** If you believe a flag is a false positive, use the **Feedback** option in the left navigation bar to report it. The Dhan team reviews feedback and updates the scanner accordingly.

---

## DhanHQ SDK

Using the official DhanHQ Python SDK in Dhan Cloud, including market data, order placement, and fund limits.

The `dhanhq` package is the official Dhan Python SDK. Add it to your Dependencies tab and use it to place orders, fetch market data, and check account balances.

SDK reference: [github.com/dhan-oss/DhanHQ-py](https://github.com/dhan-oss/DhanHQ-py)

## Initialization

```python
from dhanhq import DhanContext, dhanhq

client_id    = "{{CLIENT_ID}}"
access_token = "{{ACCESS_TOKEN}}"

dhan = dhanhq(DhanContext(client_id, access_token))
```

## Market Data

Fetch live prices for one or more instruments using their security IDs. Security IDs are Dhan's internal instrument identifiers (e.g. `1333` is Infosys on NSE).

```python
# Last traded price
quote = dhan.ticker_data(securities={"NSE_EQ": [1333]})

# OHLC (open, high, low, close)
ohlc = dhan.ohlc_data(securities={"NSE_EQ": [1333]})

# Full market depth quote
full = dhan.quote_data(securities={"NSE_EQ": [1333]})
```

## Historical Data

```python
from datetime import date, timedelta

to_date   = date.today().isoformat()
from_date = (date.today() - timedelta(days=30)).isoformat()

# Minute-level intraday candles
intraday = dhan.intraday_minute_data(
    security_id="1333",
    exchange_segment="NSE_EQ",
    instrument_type="EQUITY",
    from_date=from_date,
    to_date=to_date
)

# Daily end-of-day candles
daily = dhan.historical_daily_data(
    security_id="1333",
    exchange_segment="NSE_EQ",
    instrument_type="EQUITY",
    from_date=from_date,
    to_date=to_date
)
```

## Placing an Order

```python
dhan.place_order(
    security_id="1333",
    exchange_segment=dhan.NSE,
    transaction_type=dhan.BUY,
    quantity=1,
    order_type=dhan.MARKET,
    product_type=dhan.INTRA,
    price=0
)
```

Use these constants for `exchange_segment`, `transaction_type`, `order_type`, and `product_type`:

| Parameter | Available values |
|---|---|
| Exchange segment | `dhan.NSE`, `dhan.BSE`, `dhan.NSE_FNO`, `dhan.MCX` |
| Transaction type | `dhan.BUY`, `dhan.SELL` |
| Order type | `dhan.MARKET`, `dhan.LIMIT`, `dhan.SL`, `dhan.SLM` |
| Product type | `dhan.INTRA` (intraday), `dhan.CNC` (delivery), `dhan.MARGIN` |

## Checking Fund Limits

Returns available margin, used margin, and other balance details for your account.

```python
funds = dhan.get_fund_limits()
print(funds)
```

---

## Writing Strategies

How to write, structure, and deploy Python trading strategies on Dhan Cloud.

A strategy on Dhan Cloud is a Python script. It runs in a managed container with access to the Dhan trading APIs via the official DhanHQ SDK (`dhanhq`).

## Using the DhanHQ SDK

The DhanHQ SDK is the primary way to interact with Dhan from your strategy. It provides methods for placing orders, fetching market data, and checking account balances.

Add `dhanhq` to your Dependencies tab, then initialize it with your credentials:

```python
from dhanhq import DhanContext, dhanhq

client_id    = "{{CLIENT_ID}}"
access_token = "{{ACCESS_TOKEN}}"

dhan = dhanhq(DhanContext(client_id, access_token))
```

Full SDK reference: [github.com/dhan-oss/DhanHQ-py](https://github.com/dhan-oss/DhanHQ-py)

## Creating a Strategy

Click **New Strategy** in the dashboard to start. You can choose from three creation methods:

- **Blank** — start from an empty editor
- **Template** — pick from a set of pre-built starting points
- **Pine Script to Python** — paste a TradingView Pine Script and convert it to Python using AI

## Single-File and Multi-File Strategies

The editor supports both single-file and multi-file strategies. If your strategy spans multiple Python files, you can add them as separate files in the editor. You can also include data files such as CSV files with symbol lists or reference data.

## Minimal Strategy Structure

Every strategy needs credentials, an SDK client, and a loop:

```python
from dhanhq import DhanContext, dhanhq
import time

client_id    = "{{CLIENT_ID}}"
access_token = "{{ACCESS_TOKEN}}"

dhan = dhanhq(DhanContext(client_id, access_token))

while True:
    # your trading logic here
    time.sleep(60)
```

---

## Pine Script to Python

AI-powered converter that translates TradingView Pine Script v5 strategies into Python code for Dhan Cloud.

Dhan Cloud includes an AI-powered converter that translates TradingView Pine Script v5 strategies into Python code compatible with Dhan Cloud.

> **note:** Only Pine Script **v5** is supported. Paste the full script including the `//@version=5` header.

The converter uses a combination of fixed mapping rules and generative AI. Output code is a starting point, not production-ready. Always review the generated code before deploying with real funds.

## How to Use It

1. Click **New Strategy** and select **Pine Script to Python**
2. Paste your Pine Script v5 strategy
3. The converter maps indicators, conditions, and entry/exit logic to Python equivalents
4. Review the output, set your Variables and Dependencies, then deploy

## What Gets Converted

| Pine Script | Python equivalent |
|---|---|
| `ta.ema(close, 20)` | `pandas_ta.ema(df['close'], 20)` |
| `strategy.entry("Long", strategy.long)` | `dhan.place_order(..., transaction_type=dhan.BUY)` |
| `strategy.close("Long")` | `dhan.place_order(..., transaction_type=dhan.SELL)` |
| `request.security(...)` | `dhan.intraday_minute_data(...)` or `dhan.historical_daily_data(...)` |

## Limitations

- `strategy.risk.*` functions have no direct Python equivalent. The AI will approximate or omit them.
- Multi-timeframe logic may need manual adjustment after conversion.
- Replay and backtesting are not converted. Dhan Cloud runs strategies live only.
- Custom Pine Script functions are converted on a best-effort basis and should be tested carefully.

## Example

**Input Pine Script:**

```pinescript
//@version=5
strategy("EMA Crossover", overlay=true)
fast = ta.ema(close, 9)
slow = ta.ema(close, 21)
if ta.crossover(fast, slow)
    strategy.entry("Long", strategy.long)
if ta.crossunder(fast, slow)
    strategy.close("Long")
```

**Generated Python:**

```python
from dhanhq import DhanContext, dhanhq
import pandas as pd
import pandas_ta as ta
import time

client_id    = "{{CLIENT_ID}}"
access_token = "{{ACCESS_TOKEN}}"
dhan = dhanhq(DhanContext(client_id, access_token))

def run():
    from datetime import date, timedelta
    to_date   = date.today().isoformat()
    from_date = (date.today() - timedelta(days=5)).isoformat()
    resp = dhan.intraday_minute_data(
        security_id="13", exchange_segment="IDX_I",
        instrument_type="INDEX", from_date=from_date, to_date=to_date
    )
    df = pd.DataFrame(resp.get("data", []))
    df['fast'] = ta.ema(df['close'], length=9)
    df['slow'] = ta.ema(df['close'], length=21)

    crossover  = df['fast'].iloc[-2] < df['slow'].iloc[-2] and df['fast'].iloc[-1] > df['slow'].iloc[-1]
    crossunder = df['fast'].iloc[-2] > df['slow'].iloc[-2] and df['fast'].iloc[-1] < df['slow'].iloc[-1]

    if crossover:
        dhan.place_order(security_id="1333", exchange_segment=dhan.NSE,
                         transaction_type=dhan.BUY, quantity=1,
                         order_type=dhan.MARKET, product_type=dhan.INTRA, price=0)
    elif crossunder:
        dhan.place_order(security_id="1333", exchange_segment=dhan.NSE,
                         transaction_type=dhan.SELL, quantity=1,
                         order_type=dhan.MARKET, product_type=dhan.INTRA, price=0)

while True:
    run()
    time.sleep(60)
```

> **warning:** Always review AI-generated code before deploying with real funds. Verify order quantities, product types, and exit conditions match your intended logic.

---

## Common Errors

Common errors in Dhan Cloud strategies and how to fix them, covering deployment, auth, orders, rate limits, SDK, and Python issues.

## Deployment Errors

**`ModuleNotFoundError: No module named 'dhanhq'`** (or `requests`, `pandas`, `numpy`)

The package is not listed in Dependencies. Add it to the **Dependencies** tab:
```
dhanhq
pandas
numpy
requests
```

---

**`[pip] ERROR: Invalid requirement: 'pip install dhanhq'`**

The Dependencies tab takes package names only, not install commands. Remove `pip install` and list just the name:
```
dhanhq
```

---

**`[pip] ERROR: No matching distribution found for time`**

`time` is part of Python's standard library and cannot be installed via pip. Remove it from Dependencies. Use `import time` in your code directly.

---

**`SyntaxError`** / **`IndentationError`**

Usually caused by indentation corruption from copy-pasting in the browser editor. To fix:
1. Copy your code into a local editor (VS Code or PyCharm)
2. Fix indentation using 4 spaces, not tabs
3. Paste the corrected code back

---

**`Code validation failed` (scanner block)**

The security scanner flagged something in your code. Any flag blocks deployment. See [Code Scanner](/cloud/security/code-scanner) for how to identify and fix specific flags.

---

## Authentication Errors

**`DH-901` — access token invalid or expired**
```json
{"errorType": "Invalid_Authentication", "errorCode": "DH-901", "errorMessage": "Client ID or user generated access token is invalid or expired."}
```
Your access token has expired or does not match the Client ID. Fix:
1. Regenerate your access token at [developer.dhanhq.co](https://developer.dhanhq.co)
2. Update the `ACCESS_TOKEN` variable in your strategy
3. Confirm that `CLIENT_ID` matches your Dhan account

---

**`Login failed. Please retry with valid credentials.`**

Credentials are wrong or the login flow failed. Check `CLIENT_ID`, `ACCESS_TOKEN`, and any TOTP configuration in Variables.

---

**`Token can be generated once every 2 minutes.`**

Your strategy is trying to generate an access token too frequently. Generate it once at startup and reuse it throughout the session. Do not call token generation inside a loop.

---

**`HTTP Error 401: Unauthorized`**

The access token expired during a run or was not injected correctly. Regenerate the token and update the `ACCESS_TOKEN` variable. If you generate the token programmatically, make sure the generation call succeeds before any API calls are made.

---

## Order and Trading Errors

**`DH-906` — market is closed**
```json
{"errorType": "Order_Error", "errorCode": "DH-906", "errorMessage": "Market is Closed! Want to place an offline order?"}
```
An order was placed outside of market hours. NSE/BSE equity hours are 9:15 AM to 3:30 PM IST on weekdays. Add a market hours check:

```python
from datetime import datetime
import pytz

def is_market_open():
    now = datetime.now(pytz.timezone('Asia/Kolkata'))
    return now.weekday() < 5 and 9 * 60 + 15 <= now.hour * 60 + now.minute <= 15 * 30

if is_market_open():
    dhan.place_order(...)
```

---

**`DH-905` — missing or invalid parameters**
```json
{"errorType": "Input_Exception", "errorCode": "DH-905"}
```
A required field is missing or has an invalid value. Check that `security_id`, `exchange_segment`, `transaction_type`, `quantity`, `order_type`, and `validity` are all correct. Refer to [Order API docs](https://docs.dhanhq.co).

---

**`Order failed: Unknown error`**

An unspecified error from the order management system, usually transient. Implement retry logic with a delay. If it persists, check the full response body in logs for an `omsErrorDescription` field.

---

**`omsErrorDescription: RMS:...:You have insufficient funds`**

Insufficient margin for the order size. Reduce position size or add funds to your trading account. You can check available margin with:
```python
funds = dhan.get_fund_limits()
```

---

## Rate Limiting

**`HTTP Error 429: Too Many Requests`**
```
{"data": {"805": "Too many requests. Further requests may result in the user being blocked."}}
```
Your strategy is calling Dhan APIs too frequently. Dhan enforces per-second and per-minute rate limits on data and order endpoints. Fix:

1. Add `time.sleep()` between API calls
2. Use exponential backoff on 429 responses
3. Cache historical data instead of fetching it on every loop iteration

```python
import time

def fetch_with_retry(fn, *args, retries=3, delay=5):
    for i in range(retries):
        result = fn(*args)
        if result:
            return result
        time.sleep(delay * (2 ** i))
    return None
```

---

**`YFRateLimitError` (yfinance)**

yfinance is being called too frequently. Cache the results and reduce call frequency. For Indian market data, consider using Dhan's own data APIs instead.

---

## SDK Errors

**`'dhanhq' object has no attribute 'get_ticker_data'`** or **`'historical_data'`**

These method names changed in SDK v2. Use the current names:
- `get_ticker_data` is now `ticker_data`
- `historical_data` is now `historical_daily_data` or `intraday_minute_data`

See the [dhanhq changelog](https://github.com/dhan-oss/DhanHQ-py) for a full list of renamed methods.

---

**`'list' object has no attribute 'items'`** from `ticker_data`

`ticker_data` returned a list rather than a dict. Handle both cases:
```python
data = dhan.ticker_data(...)
if isinstance(data, list):
    for item in data:
        process(item)
elif isinstance(data, dict):
    process(data)
```

---

**`json.decoder.JSONDecodeError: Expecting value`**

The API returned an empty response body, usually because of an auth failure or rate limit. Check the HTTP status code before parsing:
```python
response = requests.get(url, headers=headers)
if response.status_code != 200 or not response.text:
    print(f"API error {response.status_code}: {response.text}")
    return None
data = response.json()
```

---

**`404 Client Error`**

The endpoint URL is wrong. Check the [API docs](https://docs.dhanhq.co) for the correct path.

---

**`Error parsing option chain: 'last_price'`**

The option chain response does not contain the key you expected. Use `.get()` with a fallback:
```python
ltp = option_data.get('last_price') or option_data.get('ltp')
if ltp is None:
    print("Unexpected response structure:", option_data.keys())
```

---

## Python and pandas Errors

**`TypeError: NDFrame.fillna() got an unexpected keyword argument 'method'`**

The `method` parameter was removed from `fillna()` in pandas 2.x. Use `ffill()` directly:
```python
df['col'].ffill()
```

---

**`pandas.errors.NullFrequencyError: Cannot shift with no freq`**

The DatetimeIndex has no frequency set. Set it explicitly or use `pct_change()` instead of shift-based calculations:
```python
df.index.freq = pd.tseries.frequencies.to_offset('1min')
# or
df['returns'] = df['close'].pct_change()
```

---

**`ChainedAssignmentError`**

pandas 2.x raises an error (not just a warning) for chained assignment. Use `.loc` instead:
```python
# Wrong
df[df['signal'] == 1]['qty'] = 10

# Correct
df.loc[df['signal'] == 1, 'qty'] = 10
```

---

**`ValueError: The truth value of a Series is ambiguous`**

You are using a pandas Series in a boolean context. Use `.any()`, `.all()`, or `.empty`:
```python
if not df['signal'].empty and df['signal'].any():
    ...
```

---

**`KeyError: 0`** or **`IndexError: single positional indexer is out-of-bounds`**

The DataFrame is empty or you are using integer indexing on a non-integer index. Always check before accessing:
```python
if not df.empty:
    row = df.iloc[0]
```

---

**`NameError: name 'X' is not defined`**

A variable is used before it is assigned. This often happens when a variable is defined inside a conditional block that was not reached. Initialize all variables at the top of your function:
```python
start_hour = 9
```

---

## Runtime Issues

**Strategy runs once and stops**

Your strategy needs a `while True` loop with `time.sleep()` to keep running:
```python
import time
while True:
    run_strategy()
    time.sleep(60)
```

---

**Orders not placing, no error in logs**

Variable syntax is wrong. Double braces are required:
```python
client_id = "{{CLIENT_ID}}"   # correct
client_id = "{CLIENT_ID}"     # wrong, treated as a literal string
```

---

**`AuthError` or `Invalid Token`**

Access token has expired. Regenerate it at [developer.dhanhq.co](https://developer.dhanhq.co) and update the `ACCESS_TOKEN` variable.

---

## Editor Issues

**Indentation errors after editing**

The browser editor can corrupt indentation in files over roughly 200 lines. Edit large files locally and paste the full content at once.

**Error traceback points to the wrong line**

Python tracebacks can be offset from the actual error. Look at the 10 lines above the reported line number for unclosed brackets, missing colons, or broken indentation.

---

## Dhan API Error Code Reference

| Error Code | Error Type | Cause |
|---|---|---|
| `DH-901` | `Invalid_Authentication` | Access token expired or Client ID mismatch |
| `DH-905` | `Input_Exception` | Missing or invalid order parameters, or rate limit hit |
| `DH-906` | `Order_Error` | Order placed outside market hours |
| `DH-907` | `Order_Error` | Insufficient funds in account |

Full reference: [docs.dhanhq.co/api/v2/guides/errors](https://docs.dhanhq.co/api/v2/guides/errors)
