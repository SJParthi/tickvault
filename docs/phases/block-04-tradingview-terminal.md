# Block 04 — TradingView Terminal Frontend

> **Status:** In Progress
> **Depends on:** Block 03 (WebSocket Connection Manager)
> **Delivers:** Browser-based charting terminal with real-time tick visualization

---

## Architecture Overview

```
Browser (SPA)                          Rust Backend (existing)
┌───────────────────────┐              ┌─────────────────────────┐
│ Lightweight Charts v5 │◄── render ───┤                         │
│ + Custom Renderers    │              │ Axum WS /ws/ticks       │
│                       │              │   └─ broadcast channel   │
│ WASM Indicator Engine │◄── compute ──┤                         │
│   └─ Plugin Registry  │              │ GET /api/candles        │
│                       │              │   └─ QuestDB SAMPLE BY  │
│ TypeScript UI Shell   │              │                         │
│   └─ Toolbar/Panels   │              │ GET /api/instruments    │
└───────────────────────┘              └─────────────────────────┘
```

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Charting engine | TradingView Lightweight Charts v5 | Same renderer as TradingView. Apache-2.0. 35KB. Native multi-pane. |
| Indicator compute | Rust compiled to WASM | O(1) enforced at compile time. Shared with backend. Zero GC. |
| UI framework | None (vanilla TypeScript) | Minimal bundle. No framework overhead. Direct DOM control. |
| Bundler | esbuild | Fastest bundler. Single binary. No node_modules bloat. |
| Indicator architecture | Plugin registry pattern | Dynamic add/remove at runtime. O(1) registry lookup. |
| Data transport | Binary WebSocket (same as Dhan V2) | No JSON serialization overhead. Reuses existing parser. |
| Historical data | QuestDB SAMPLE BY | Server-side candle aggregation. Single query per timeframe switch. |
| Frontend hosting | Axum static file serving | Same container. No separate Node server. |

## Component Inventory

### Backend Additions (Rust)

1. **`crates/api/src/handlers/ws_broadcast.rs`** — WebSocket upgrade + tick broadcast
2. **`crates/api/src/handlers/candles.rs`** — Historical OHLCV from QuestDB
3. **Broadcast channel** in tick processor — fan-out to WS clients

### WASM Crate

4. **`crates/frontend-wasm/`** — Rust → WASM indicator engine
   - `src/lib.rs` — wasm_bindgen entry point
   - `src/registry.rs` — indicator plugin registry
   - `src/pipeline.rs` — active instance orchestration
   - `src/aggregator.rs` — tick → candle (any timeframe)
   - `src/indicator.rs` — Compute trait definition
   - `src/indicators/*.rs` — individual indicator implementations

### Frontend (TypeScript)

5. **`frontend/`** — browser application
   - `src/main.ts` — entry point
   - `src/chart-manager.ts` — Lightweight Charts v5 lifecycle
   - `src/ws-client.ts` — binary WebSocket client
   - `src/wasm-bridge.ts` — typed WASM interface
   - `src/pipeline-manager.ts` — compute → render orchestration
   - `src/render-registry.ts` — renderer plugin registry
   - `src/renderers/*.ts` — indicator-specific renderers
   - `src/ui/*.ts` — toolbar, indicator picker, order panel

## Indicator Plugin Architecture

### Compute Trait (Rust/WASM)

```rust
pub trait Compute: Send {
    fn on_candle(&mut self, o: f64, h: f64, l: f64, c: f64, v: f64) -> &[f64];
    fn reset(&mut self);
    fn output_count(&self) -> usize;
}
```

### Renderer Interface (TypeScript)

```typescript
interface IndicatorRenderer {
    readonly type: "overlay" | "pane";
    attach(chart: IChartApi, pane?: IPaneApi): void;
    update(output: Float64Array, time: number): void;
    detach(): void;
}
```

### Adding a New Indicator

1. Create `crates/frontend-wasm/src/indicators/new_indicator.rs`
2. Implement `Compute` trait with O(1) `on_candle()`
3. Register in `registry.rs`: `registry.register("name", factory)`
4. Create `frontend/src/renderers/new-renderer.ts`
5. Register in `render-registry.ts`
6. Done — auto-appears in indicator picker dropdown

## Data Flow

### Live Tick Path (O(1) per tick)
```
Dhan WS → Parser → TickProcessor → broadcast::Sender → Axum WS
                         │                                  │
                    QuestDB persist                   Browser WS
                                                          │
                                                    WASM aggregator
                                                          │
                                                  ┌───────┴───────┐
                                                  │               │
                                            candle update   indicator.on_candle()
                                                  │               │
                                            chart.update()   renderer.update()
```

### Historical Load Path (one-shot per symbol/timeframe change)
```
User selects timeframe → GET /api/candles?symbol=X&tf=1m&from=T1&to=T2
                                    │
                              QuestDB: SELECT ... FROM ticks SAMPLE BY 1m
                                    │
                              JSON response → chart.setData()
                                    │
                              WASM: replay candles → warm indicator state
                                    │
                              Renderers: setData() for all overlays/panes
```

## Timeframes

All derived from raw ticks in QuestDB via `SAMPLE BY`:

| Timeframe | QuestDB Query |
|-----------|---------------|
| 1s | `SAMPLE BY 1s` |
| 1m | `SAMPLE BY 1m` |
| 5m | `SAMPLE BY 5m` |
| 15m | `SAMPLE BY 15m` |
| 1h | `SAMPLE BY 1h` |
| 1d | `SAMPLE BY 1d` |

All aligned to IST via `ALIGN TO CALENDAR WITH OFFSET '05:30'`.

## Performance Guarantees

| Metric | Target | Mechanism |
|--------|--------|-----------|
| Tick → screen | < 1ms | Binary WS + WASM parse + Canvas update |
| Indicator per candle | O(1) | Pre-allocated ring buffers, Compute trait |
| Chart render | < 16ms (60 FPS) | Canvas 2D viewport culling |
| Memory | Bounded | Fixed-size ring buffers, no unbounded growth |
| Bundle size | < 200KB (JS+WASM) | esbuild tree-shaking, wasm-opt -Oz |
