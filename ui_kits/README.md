# Tickvault Design System

A design system distilled from the **tickvault** codebase — an O(1)-latency live F&O trading backend for the Indian NSE market, built in Rust 2024 Edition. The product is marketed internally as **DLT (dhan-live-trader)** and exposes a small set of operator dashboards served directly from the `crates/api` Axum handler. This design system captures the **terminal-dense, zero-fluff, trader-console aesthetic** of those dashboards so new surfaces (internal tools, docs, decks, marketing pages) can be built consistently.

## Sources

All visual and copy decisions in this system were distilled from the tickvault GitHub repo at `SJParthi/tickvault@main`. No Figma file exists; the codebase is the source of truth.

Pages studied:
- `crates/api/static/portal.html` — the DLT Control Panel (home grid)
- `crates/api/static/market-dashboard.html` — live market data tables + watchlist
- `crates/api/static/ws-dashboard.html` — WebSocket pool / subsystem health
- `crates/api/static/option-chain-v2.html` — option chain with Greeks
- `crates/api/static/options-chain.html`, `markets-index.html`, `markets-stocks.html`, `markets-options.html` — additional trading screens (style vocabulary cross-checked)

Supporting docs read for context:
- `README.md`, `CLAUDE.md` — positioning, architecture, "three principles"
- `docs/architecture/codebase-map.md` — module layout
- `docs/flow-diagrams.md`, `docs/flow-functional.md` — data-flow conventions

## Product summary

Tickvault ingests binary WebSocket frames from Dhan (NSE broker), parses them in O(1) via fixed-offset `from_le_bytes` reads, runs them through a zero-alloc pipeline (junk filter → dedup ring → candle aggregation → top movers), and persists ticks to QuestDB via ILP. The operator-facing surface is a set of **dark, dense, data-forward dashboards** — much closer to a Bloomberg terminal or Grafana than a consumer fintech app. Three operator-facing palettes co-exist in the live code (`#0a0a0a` portal, `#000000` market dashboard, `#0d1117` ws/options dashboards); this design system picks the **GitHub-dark `#0d1117`** palette used by the most recent screens (ws-dashboard, option-chain-v2) as the canonical one, while preserving the NSE-standard green/red semantics shared by all three.

## Index

Root files
- **README.md** (this file) — overview, content + visual foundations, iconography, font fallback
- **colors_and_type.css** — all tokens (colors, type, spacing, radii, shadows, semantic vars). Import this before anything else.
- **SKILL.md** — front-matter skill manifest so Claude Code can invoke this system; short "when to use / how to use" summary.

Folders
- **assets/** — `logo-dlt-chip.svg` (brand chip), `avatar-purple.svg` (user avatar), `wordmark.svg` (horizontal wordmark).
- **preview/** — 17 single-concept cards powering the Design System tab, grouped Colors / Type / Components / Spacing / Brand. Not meant to be read top-to-bottom — browse via the tab.
- **ui_kits/dashboard/** — `index.html`: a pixel-close re-assembly of the four live tickvault screens (chrome + ticker, control panel, WS dashboard, markets, option chain) using only tokens from `colors_and_type.css`. Lift sections from here when building new surfaces.

Suggested read order for a new designer: README → colors_and_type.css → ui_kits/dashboard/index.html → preview/ as reference.

---

## CONTENT FUNDAMENTALS

Tickvault's copy reads like **operator notes from an HFT engineer**, not a marketing team. It is terse, technical, and data-first.

**Voice.** Third-person technical, present tense. The system describes itself ("O(1) F&O Trading System", "Zero allocation on hot path", "2439 tests passing"). Second person ("you") is avoided. First person never appears. When the reader is addressed, it is through imperatives in runbooks ("Run bootstrap.sh", "Copy & paste into QuestDB console").

**Casing.** Mixed deliberately:
- Headings and card titles: Title Case ("Market Dashboard", "Option Update WS")
- Status labels and badges: ALL CAPS, often with 0.5–2px letter-spacing ("LIVE", "DOWN", "CONNECTED", "BUFFERING", "DATABASE", "MONITORING")
- Section headers in portals: ALL CAPS with 2px tracking ("📊 APPLICATION", "🗄️ DATABASE & STORAGE")
- Inline data labels: lowercase, sometimes abbreviated ("max 5 connections", "single connection", "24h validity")
- Code / endpoints: lowercase, monospace (`wss://api-feed.dhan.co`, `redis-cli -h localhost -p 6379`)

**Density.** Screens are dense. Every card carries at least 3 pieces of information (title, description, URL/endpoint, badge). There is no empty space used for "breathing room" — if a column is quiet, it shows `--` dashes, not placeholder text.

**Specifics over abstractions.** Copy always favors concrete numbers:
- Not "a few tests" → "2439 tests passing"
- Not "high throughput" → "5,000 / conn · 100 / msg"
- Not "real time" → "Auto-refresh: 3s"
- Not "secure" → "24h validity, arc-swap refresh"

**Abbreviations.** Heavy use of domain jargon without glossing: LTP, OI, IV, PCR, ATM, CE/PE, FUT, F&O, ILP, JWT, WS, OMS, OBI, SPSC, WAL, NSE, BSE. Abbreviations always preferred over long forms once established. The audience is assumed to know finance + systems.

**Error copy.** Deadpan. "API: DOWN", "Failed to load data. Retrying...", "No stock-only data available. API returned 12 items (derivatives/futures excluded)." Never apologetic, never cute.

**Numbers.** Indian-style formatting where domain-appropriate: prices in ₹ with two decimals and `en-IN` grouping ("1,372.90"), OI in lakhs ("12.45L" once above 100,000), volumes in full with commas, monetary value in Crores ("Cr."). Greeks shown to 3–4 decimal places. Percentages always get a sign (`+1.44%`, `-0.48%`).

**Emoji.** Used sparingly and only as **section markers in portals** (📊 🗄️ 📝 🔄 🔌 🏗️ 🔑 ❤️ 🔎) — never in body copy, never decorative, never in UI kits or trading screens. The `option-chain-v2.html` and `ws-dashboard.html` use zero emoji. Treat emoji as optional; prefer a leading `//` or an arrow glyph (`→`) for section dividers in new surfaces.

**Signature phrases.**
- "O(1) latency", "O(1) or fail at compile time", "Zero allocation on hot path"
- "Verified 2026-04-06" (date-stamped verification callouts on reference tables)
- "independent per WS type", "max N connections"
- "Pure tick capture backend"
- IST is always called out explicitly — there is no assumption of local time.

---

## VISUAL FOUNDATIONS

### Canonical palette (from ws-dashboard.html / option-chain-v2.html)

| Role | Hex | Use |
|---|---|---|
| `--bg` | `#0d1117` | Page background |
| `--bg-card` | `#161b22` | Card / header / table-head / ticker-bar |
| `--bg-card-hover` | `#1c2228` | Interactive hover |
| `--border` | `#30363d` | All dividers, card borders, table cell borders |
| `--text` | `#e6edf3` | Primary text, numeric values |
| `--text-dim` | `#8b949e` | Labels, secondary text, nav |
| `--text-muted` | `#484f58` | De-emphasized / disabled |
| `--green` | `#26a69a` | Positive change, up status, CE side |
| `--green-strong` | `#05b878` | Active nav / "LIVE" indicators (market-dashboard) |
| `--red` | `#ef5350` | Negative change, down status, PE side |
| `--red-soft` | `#dd6565` | Down values in table body |
| `--blue` | `#2962ff` | Brand / primary accent / links / ATM highlight |
| `--yellow` | `#ffab00` | Warnings / buffering / credential callouts |
| `--purple` | `#7c4dff` | Database / secondary accent |

Transparency versions are used as **tinted backgrounds behind colored badges** — always 15% opacity of the underlying color:
- `rgba(38,166,154,0.15)` → green badge bg
- `rgba(239,83,80,0.15)` → red badge bg
- `rgba(255,171,0,0.15)` → yellow badge bg
- `rgba(41,98,255,0.15)` → blue badge bg + ATM strike highlight
- `rgba(124,77,255,0.15)` → purple badge bg
- `rgba(38,166,154,0.06)` / `rgba(239,83,80,0.06)` → whole-row CE/PE tint in option chain

### Type

**Families.**
- **UI sans** — Inter, with system fallbacks (`-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif`). Used for everything except numeric data and endpoints.
- **Mono** — `'SF Mono', Monaco, Consolas, 'Cascadia Code', monospace`. In the option chain, the ENTIRE screen is mono-set (dense Greeks table reads better that way). Elsewhere mono is reserved for clock, URLs/endpoints, code snippets, and the live ticker count.

In this system we standardize on **Inter** (var) for UI and **JetBrains Mono** for mono — both loaded via Google Fonts (see FONT FALLBACK section below).

**Scale** (extracted from the static HTMLs):
- 28px / 300 weight / +2px tracking — portal H1 (the single largest thing)
- 16px / 600 — panel headers ("DLT WebSocket Dashboard", card titles)
- 14px / 400 — base body / table cells
- 13px / 500 — secondary body, sidebar items, tab labels
- 12px / 400 — tickers, sub-labels, filter buttons
- 11px / 600 / +0.5–2px tracking / uppercase — section headers, badge labels, stat labels
- 10px / 700 / uppercase — badges
- 10–11px / 400 — status hints, "max 5 connections", "checking…"

**Numeric rendering.** Big stat values use `font-weight: 700`, color-coded green (positive / present) or red (error). Table numbers are right-aligned; first column (name) is left-aligned, slightly brighter than the numeric columns (`--text` vs `--text-dim`).

### Spacing

No formal 4/8 scale in the source — spacing is ad hoc but clusters around these values: **4, 6, 8, 10, 12, 14, 16, 20, 24, 30, 40 px.** This system codifies a **4-point scale (`--space-1` = 4px through `--space-10` = 40px)** and uses those as defaults, but leaves room to match existing screens where needed.

### Backgrounds

- **Flat solid colors only.** No gradients on backgrounds, no textures, no repeating patterns, no hand-drawn imagery, no hero illustrations.
- Two gradient accents exist and are the only gradients permitted: the brand logo chip (`linear-gradient(135deg, #1e90ff, #4169e1)` — a deep-blue orb) and the user avatar (`linear-gradient(135deg, #9370db, #6a4c93)` — purple). Both are applied to 30–32px round swatches only.
- Full-bleed images are **not** used. Imagery, when it appears, is functional (a chart canvas, a sparkline, a depth ladder) and rendered from live data.
- Cards are raised by a 1px border + subtle fill shift — **not** by shadow.

### Borders & dividers

- Everything separates via a **1px solid `--border` (`#30363d`)** line.
- `border-radius` values are tight: **4px** on buttons and small inputs, **6px** on compact stat cards, **8px** on primary cards (control-panel tiles), **0** on table rows and sticky headers.
- The market-dashboard sidebar uses a **3px color bar** on the left of each item (green / red) as a status indicator — this is a reusable motif.
- Tables use **0.5–1px bottom borders per row** and a **2px bottom border on `<thead>`**.

### Shadows

Shadows are **rare and only used on hover** for the big portal cards:

```
box-shadow: 0 4px 12px rgba(41, 98, 255, 0.15);  /* card hover lift */
```

Everything else is flat. No inner shadows. No protection gradients. No drop shadows on text.

### Hover / press states

- **Background lightening**, not highlighting. `#161b22` → `#1c2228`, or `#000` → `#0a0a0a`.
- **Border colorizing.** Card hover moves the border from `--border` to the semantic accent (`--blue` for portal, `--green` for connected pools).
- **Micro translate + shadow.** The portal cards lift `translateY(-2px)` on hover with a 4/12 blue shadow.
- **Opacity dim** for dim-tables: `tr:hover { background: rgba(255,255,255,0.03); }` — a 3% white wash, extremely subtle.
- **Sort icons** on table headers go from `opacity: 0.5` to `1` on `th:hover`.
- **Press state** is not styled explicitly — the button simply stays in its hover state until release. No shrink, no shadow collapse.

### Transparency & blur

- The market-dashboard top-nav uses `rgba(18, 18, 18, 0.62)` — a 62% opaque dark band. This is the only place blur-like transparency appears, and there is **no `backdrop-filter`**. Use it sparingly for layered nav/ticker bars.
- Badge tinted backgrounds (15% opacity colored washes) are the other legitimate use of transparency.

### Animation

- **Animations are minimal and utilitarian.** All transitions are `0.15s ease` or `0.2s` (0.3s for meter bars). Never bouncy, never spring.
- The only keyframe animation in the whole codebase is a **pulsing live dot** used on the nav `LIVE` indicator and the `live-dot` in status bars:
  ```
  @keyframes pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.5; } }
  ```
  2s period, infinite. Also a `shimmer` keyframe used on loading-row text (0.3 → 0.7 opacity, 1.5s).
- No fades on page transitions. No staggered list reveals. No bounce. No parallax.
- Auto-refresh on data panels — 3s, 5s, 10s, 30s intervals are all present; pick based on data volatility.

### Layout rules

- **Top nav: 60px fixed height.** Contains logo-chip (30px circle), search (220px), nav links (28px gap), right-side live indicator + user avatar.
- **Secondary ticker/index bar: ~40px.** Horizontally scrollable, pipe-separated items.
- **Sidebar (watchlist): 348px default, 280px min.** Left-pinned, bordered on the right.
- **Content tabs.** Two tab styles co-exist:
  - **Pill tabs** — market-dashboard style, a 36px-tall row of `#181818` boxes, active tab goes green with a green border and 8px radius.
  - **Underline tabs** — ws-dashboard / option-chain style, simple bottom-border-2px on active, blue tint on text. Our system treats underline tabs as the default and pill tabs as a variant for primary surface-switching.
- **Sub-tabs.** A row of small pill-chips (`--bg-card` filled, `--blue` border when active) or plain underlines.
- **Filter bar.** Segmented button groups side-by-side, connected by a shared 1px border.
- **Data tables.**
  - Sticky `<thead>` with `--bg-card` fill and 2px bottom border.
  - First column left-aligned and color `--text`.
  - Numeric columns right-aligned and color `--text-dim`, colored green/red when signed.
  - Row height 42px in market-dashboard, 28–32px in denser ws-dashboard / option-chain.
- **Status bar** at the bottom (ws-dashboard): 6px padding, `--bg-card` fill, links in blue, live-dot on the left.

### Cards

- Surface `--bg-card` + 1px `--border`, 6–8px radius.
- Padding: 14px on stat tiles, 20px on primary tiles.
- Hover: border goes blue, bg nudges to `--bg-card-hover`, translate -2px, soft blue shadow.
- Title 16px/600, description 12px dim, optional URL/endpoint in 11px blue mono, optional badge in the bottom-left.

### Imagery mood

Functional-only. Everything that looks like "imagery" is in fact a data viz — ticker bars, meter fills, colored status dots. If a real photo were ever needed (e.g. a marketing page, which doesn't exist today), keep it **cool-toned, underexposed, high-contrast**, consistent with the black/navy workbench vibe. Never warm, never pastel, never hand-drawn.

---

## ICONOGRAPHY

Tickvault does **not** ship an icon system. The operator dashboards use three overlapping approaches — we catalog them so you can pick the right one:

1. **HTML entity / unicode glyphs.** The market-dashboard uses `&#9881;` (gear), `&#9662;` (chevron down), `&#128269;` (magnifying glass), `&#8943;` (horizontal ellipsis), `&#9650;` / `&#9660;` (up/down arrows for sort). This keeps the dashboard zero-dependency.
2. **Emoji as section markers.** The portal uses single-glyph emoji for top-level section dividers only: 📊 (markets / grafana), 🗄️ (database), 📝 (logs), 🔄 (collector), 🔌 (websocket), 🏗️ (infrastructure), 🔑 (credentials), ❤️ (health), 🔎 (trace/search). Emoji never appear in labels, buttons, or body copy.
3. **Colored status dots + badges.** A 6–8px round `background` swatch is used for connection / subsystem status everywhere. The bar-meter component (`.meter-bar` + `.meter-fill`) plays the same role for pool utilization. This is the primary "icon" in the system.

**No icon font is installed** (no Font Awesome, Lucide, Material Symbols, Heroicons in `package.json` — there is no `package.json`; everything is inline CSS on a single HTML file).

**Recommendation for new surfaces.** When an icon is genuinely needed and a unicode glyph isn't expressive enough, load **[Lucide](https://lucide.dev)** from CDN as `<i data-lucide="name">` — its stroke weight (2px) and rounded-line geometry match the dashboard's sharp-but-not-harsh feel. We FLAG this as a substitution: tickvault itself does not ship Lucide, so the design system adds it as an opt-in.

SVG logos (the D-chip, the purple avatar) are copied into `assets/` as standalone SVGs so they can be dropped into new layouts without re-creating the gradients.

---

## FONT FALLBACK (flagged substitutions)

- **Inter** — not shipped by tickvault; the code falls back to the system stack (`-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto`). We load **Inter** from Google Fonts as the canonical choice because the market-dashboard explicitly names `Inter` first.
- **JetBrains Mono** — not shipped; the code uses `'SF Mono', Monaco, Consolas, 'Cascadia Code'`. We substitute **JetBrains Mono** as the nearest open-source equivalent (2px stroke, clear zero, same x-height as SF Mono). **FLAG**: if you have SF Mono rights or a corporate font, swap it in.

Both fonts are loaded via `<link>` in `colors_and_type.css` — no local `.ttf` files are needed. If you want local files, drop them in `fonts/` and update the `@font-face` rules.
