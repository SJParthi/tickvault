---
paths:
  - "crates/**/*.rs"
  - "config/**/*.toml"
---

# Market Hours & Trading Rules

## Key Facts
- Timezone: ALWAYS Asia/Kolkata (IST)
- Data collection window: 9:00-16:00 IST
- Trading window: 9:15-15:29 IST
- NEVER submit orders at or after 15:30 IST
- 375 candles/day (9:15-15:30, 1-minute)
- Pre/post market data stored separately

## Implementation Rules
- ALL timestamps include IST alongside any UTC
- Market hour validation uses config values, NOT hardcoded times
- Holiday calendar checked before every trading day
- Full reference: `docs/reference/market_hours.md`
