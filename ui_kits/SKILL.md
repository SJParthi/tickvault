---
name: tickvault-design-system
description: Design system distilled from the tickvault (DLT — dhan-live-trader) codebase. Use when building operator dashboards, trading screens, internal tools, docs, or marketing surfaces that must match the tickvault aesthetic — dark (#0d1117), dense, terminal-style, NSE green/red semantics, Inter + JetBrains Mono.
---

# Tickvault Design System — Skill

This skill packages the tickvault design system. Invoke it when the user asks for any surface that should look like the tickvault operator dashboards (portal, market-dashboard, ws-dashboard, option-chain-v2) or that should feel like "the DLT trading console".

## When to use this skill

Use it when the user:
- Says "tickvault", "DLT", "dhan-live-trader", or references any of the dashboards above.
- Asks for an operator/admin console, a trading dashboard, an option chain, a watchlist, a WebSocket health panel, or a QuestDB-adjacent tool.
- Asks for a Bloomberg-terminal-style or Grafana-style UI and you have no other brand to anchor on.
- Asks for internal docs, runbooks, or decks that should share visual language with the product.

Do **not** use it for:
- Consumer-facing retail-trader apps (warmer, softer palette).
- Marketing sites that want "friendly fintech" — this system is deliberately operator-facing.

## How to use it

1. Import the tokens — always the first step:
   ```html
   <link rel="stylesheet" href="colors_and_type.css">
   ```
   All colours, type, spacing, radii, and the two allowed gradients live here. Reference variables (`var(--tv-blue)`, `var(--tv-bg-card)`, etc.) rather than hardcoding hex.

2. Read **`README.md`** top-to-bottom before making design decisions — especially the CONTENT FUNDAMENTALS section. Voice is third-person, terse, specific. Numbers always beat adjectives.

3. Browse **`preview/`** for single-concept reference cards (colors, type, components, spacing, brand). Each card is a self-contained snippet you can copy into a new layout.

4. Start from **`ui_kits/dashboard/index.html`** — it's a live compendium of real screens (chrome, control panel, WS dashboard, markets, option chain) composed only from tokens. Lift sections verbatim and edit; do not re-derive patterns from scratch.

5. Assets live in **`assets/`**:
   - `logo-dlt-chip.svg` — 30 px gradient-blue brand chip
   - `avatar-purple.svg` — 32 px gradient-purple user avatar
   - `wordmark.svg` — horizontal wordmark for doc headers

## Non-negotiables

- **Palette.** Primary surface is `#0d1117`. Only "markets" screens step to pure `#000`. Never introduce a third base. Only two gradients exist and both are reserved for the logo chip and user avatar.
- **Type.** Inter for UI, JetBrains Mono for numbers/endpoints. No third family. Scale is 10–28 px; nothing bigger.
- **Motion.** 0.15 s ease transitions only. One keyframe: the 2 s `pulse` on live dots. No spring, no stagger, no route fades, no `backdrop-filter`.
- **Semantics.** Up = `--tv-green` (#26a69a), down = `--tv-red` (#ef5350). Every status word gets a colour; every live indicator gets a pulsing dot.
- **Density.** Right-align all numbers. Show `--` for absent data, never placeholder prose. 11 px uppercase section headers with 2 px tracking.
- **Icons.** Unicode glyphs first; Lucide allowed when a glyph is insufficient; nothing else. Emoji only as section markers in the portal.

## Flagged substitutions

The tickvault code does not ship fonts or an icon set. This system specifies:
- **Inter** (canonical UI sans) and **JetBrains Mono** (canonical mono) loaded from Google Fonts — the source code falls back to system stacks, so swap in SF Mono / a licenced corporate font if available.
- **Lucide** icons as an opt-in CDN load for surfaces that need more than unicode glyphs.

When in doubt, match the existing dashboards exactly. When the answer is ambiguous, pick the denser, more numeric option — that's the tickvault way.
