/**
 * WASM Bridge — Typed interface to the Rust WASM indicator engine.
 *
 * Provides TypeScript types matching the WASM module's API and a lazy loader.
 * Falls back gracefully when WASM is not available (dev mode).
 */

// ---------------------------------------------------------------------------
// Types (mirror Rust IndicatorMeta)
// ---------------------------------------------------------------------------

export interface OutputMeta {
    name: string;
    color: string;
    style: "line" | "histogram" | "stepline" | "marker" | "band" | "rectangle";
}

export interface ParamMeta {
    name: string;
    kind: "int" | "float" | "bool";
    default: number;
    min?: number;
    max?: number;
}

export interface IndicatorMeta {
    id: string;
    display_name: string;
    display: "overlay" | "pane";
    outputs: OutputMeta[];
    params: ParamMeta[];
}

// ---------------------------------------------------------------------------
// Hardcoded metadata (available even without WASM)
// ---------------------------------------------------------------------------

/** Static indicator metadata — matches what WASM registry returns. */
export const INDICATOR_CATALOG: IndicatorMeta[] = [
    {
        id: "supertrend",
        display_name: "Supertrend",
        display: "overlay",
        outputs: [
            { name: "Line", color: "#26a69a", style: "stepline" },
            { name: "Direction", color: "#787b86", style: "line" },
            { name: "Signal", color: "#ffab00", style: "marker" },
        ],
        params: [
            { name: "Period", kind: "int", default: 10, min: 1, max: 200 },
            { name: "Multiplier", kind: "float", default: 3.0, min: 0.1, max: 20.0 },
        ],
    },
    {
        id: "smc",
        display_name: "Smart Money Concepts",
        display: "overlay",
        outputs: [
            { name: "Signal", color: "#ffab00", style: "marker" },
            { name: "Structure", color: "#787b86", style: "marker" },
            { name: "Level", color: "#2962ff", style: "line" },
            { name: "Trend", color: "#787b86", style: "line" },
        ],
        params: [
            { name: "Swing Length", kind: "int", default: 5, min: 2, max: 50 },
        ],
    },
    {
        id: "squeeze_momentum",
        display_name: "Squeeze Momentum",
        display: "pane",
        outputs: [
            { name: "Momentum", color: "#26a69a", style: "histogram" },
            { name: "Squeeze", color: "#787b86", style: "marker" },
        ],
        params: [
            { name: "BB Length", kind: "int", default: 20, min: 5, max: 100 },
            { name: "BB Mult", kind: "float", default: 2.0, min: 0.5, max: 5.0 },
            { name: "KC Length", kind: "int", default: 20, min: 5, max: 100 },
            { name: "KC Mult", kind: "float", default: 1.5, min: 0.5, max: 5.0 },
        ],
    },
    {
        id: "macd_mtf",
        display_name: "MACD MTF",
        display: "pane",
        outputs: [
            { name: "MACD", color: "#2962ff", style: "line" },
            { name: "Signal", color: "#ff6d00", style: "line" },
            { name: "Histogram", color: "#26a69a", style: "histogram" },
            { name: "Zero", color: "#434651", style: "line" },
        ],
        params: [
            { name: "Fast", kind: "int", default: 12, min: 2, max: 100 },
            { name: "Slow", kind: "int", default: 26, min: 2, max: 200 },
            { name: "Signal", kind: "int", default: 9, min: 2, max: 50 },
        ],
    },
];

// ---------------------------------------------------------------------------
// WASM Bridge Interface
// ---------------------------------------------------------------------------

/** Typed wrapper around the WASM engine. */
export interface WasmBridge {
    listIndicators(): IndicatorMeta[];
    addIndicator(id: string, params: number[]): number;
    removeIndicator(instanceId: number): boolean;
    onTick(epochSecs: number, price: number, volume: number): boolean;
    closedCandle(): number[] | null;
    currentCandle(): number[] | null;
    indicatorOutput(instanceId: number): number[];
    activeCount(): number;
    setTimeframe(seconds: number): void;
    reset(): void;
}

// ---------------------------------------------------------------------------
// WASM Loader
// ---------------------------------------------------------------------------

/**
 * Attempts to load the WASM engine from the global scope.
 *
 * The WASM module is loaded via a separate <script> tag in index.html.
 * When loaded, it sets `window.dltWasm` to the WasmEngine instance.
 */
export function getWasmBridge(): WasmBridge | null {
    const engine = (window as unknown as Record<string, unknown>)["dltWasm"];
    if (!engine) return null;

    // Type-safe wrapper around the raw WASM engine
    const wasm = engine as Record<string, (...args: unknown[]) => unknown>;
    return {
        listIndicators(): IndicatorMeta[] {
            const json = wasm["list_indicators"]() as string;
            return JSON.parse(json) as IndicatorMeta[];
        },
        addIndicator(id: string, params: number[]): number {
            return wasm["add_indicator"](id, new Float64Array(params)) as number;
        },
        removeIndicator(instanceId: number): boolean {
            return wasm["remove_indicator"](instanceId) as boolean;
        },
        onTick(epochSecs: number, price: number, volume: number): boolean {
            return wasm["on_tick"](epochSecs, price, volume) as boolean;
        },
        closedCandle(): number[] | null {
            const arr = wasm["closed_candle"]() as Float64Array;
            return arr.length > 0 ? Array.from(arr) : null;
        },
        currentCandle(): number[] | null {
            const arr = wasm["current_candle"]() as Float64Array;
            return arr.length > 0 ? Array.from(arr) : null;
        },
        indicatorOutput(instanceId: number): number[] {
            const arr = wasm["indicator_output"](instanceId) as Float64Array;
            return Array.from(arr);
        },
        activeCount(): number {
            return wasm["active_count"]() as number;
        },
        setTimeframe(seconds: number): void {
            wasm["set_timeframe"](seconds);
        },
        reset(): void {
            wasm["reset"]();
        },
    };
}
