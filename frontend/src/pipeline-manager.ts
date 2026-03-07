/**
 * Pipeline Manager — orchestrates tick → candle → indicator → render flow.
 *
 * Receives raw ticks from WsClient, aggregates into candles,
 * and dispatches to active indicator renderers via IndicatorController.
 *
 * When WASM is available, uses WASM for both aggregation and indicator
 * computation. Falls back to JS-side aggregation without indicators.
 */

import { ChartManager, type OhlcvCandle } from "./chart-manager";
import { type IndicatorController } from "./indicator-controller";
import { type RawTick } from "./ws-client";
import { type WasmBridge } from "./wasm-bridge";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface CandleBuilder {
    time: number;
    open: number;
    high: number;
    low: number;
    close: number;
    volume: number;
}

// ---------------------------------------------------------------------------
// Pipeline Manager
// ---------------------------------------------------------------------------

export class PipelineManager {
    private chartManager: ChartManager;
    private currentCandle: CandleBuilder | null = null;
    private timeframeSecs = 60; // 1-minute default
    private wasm: WasmBridge | null = null;
    private indicatorController: IndicatorController | null = null;

    /** IST offset in seconds (+5:30). */
    private static readonly IST_OFFSET = 19800;

    constructor(chartManager: ChartManager) {
        this.chartManager = chartManager;
    }

    /** Sets the WASM bridge for indicator computation. */
    setWasmBridge(wasm: WasmBridge | null): void {
        this.wasm = wasm;
    }

    /** Sets the indicator controller for renderer updates. */
    setIndicatorController(controller: IndicatorController): void {
        this.indicatorController = controller;
    }

    /** Process a single live tick. */
    onTick(tick: RawTick): void {
        if (this.wasm) {
            this.onTickWasm(tick);
        } else {
            this.onTickJs(tick);
        }

        // Update status bar OHLCV display
        this.updateOhlcvDisplay(tick);
    }

    /** Load historical candles from backend. */
    onHistoricalCandles(candles: OhlcvCandle[]): void {
        this.chartManager.setData(candles);
        this.chartManager.scrollToRealtime();
    }

    /** Change the candle timeframe (in seconds). */
    setTimeframe(seconds: number): void {
        this.timeframeSecs = seconds;
        this.currentCandle = null;

        if (this.wasm) {
            this.wasm.setTimeframe(seconds);
        }
    }

    /** Get current timeframe in seconds. */
    getTimeframe(): number {
        return this.timeframeSecs;
    }

    // ---------------------------------------------------------------------------
    // Private — WASM path
    // ---------------------------------------------------------------------------

    /**
     * Tick processing via WASM engine.
     * WASM handles aggregation + indicator computation.
     */
    private onTickWasm(tick: RawTick): void {
        if (!this.wasm) return;

        const candleClosed = this.wasm.onTick(tick.timestamp, tick.ltp, tick.volume);

        // Update chart with current (building) candle
        const current = this.wasm.currentCandle();
        if (current) {
            this.currentCandle = {
                time: current[0],
                open: current[1],
                high: current[2],
                low: current[3],
                close: current[4],
                volume: current[5],
            };
            this.chartManager.updateCandle(this.currentCandle);
        }

        // If candle closed, update indicators
        if (candleClosed && this.indicatorController) {
            const closed = this.wasm.closedCandle();
            if (closed) {
                this.indicatorController.updateAll(closed[0]);
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Private — JS fallback path (no WASM)
    // ---------------------------------------------------------------------------

    /** Tick processing via JavaScript (fallback when WASM not available). */
    private onTickJs(tick: RawTick): void {
        const aligned = this.alignTimestamp(tick.timestamp);

        if (this.currentCandle === null) {
            this.currentCandle = {
                time: aligned,
                open: tick.ltp,
                high: tick.ltp,
                low: tick.ltp,
                close: tick.ltp,
                volume: tick.volume,
            };
        } else if (this.currentCandle.time === aligned) {
            if (tick.ltp > this.currentCandle.high) this.currentCandle.high = tick.ltp;
            if (tick.ltp < this.currentCandle.low) this.currentCandle.low = tick.ltp;
            this.currentCandle.close = tick.ltp;
            this.currentCandle.volume = tick.volume;
        } else {
            this.currentCandle = {
                time: aligned,
                open: tick.ltp,
                high: tick.ltp,
                low: tick.ltp,
                close: tick.ltp,
                volume: tick.volume,
            };
        }

        this.chartManager.updateCandle(this.currentCandle);
    }

    // ---------------------------------------------------------------------------
    // Private — Utilities
    // ---------------------------------------------------------------------------

    /**
     * Aligns a UTC epoch timestamp to the start of its timeframe bucket.
     * Applies IST offset before bucketing to align to IST clock boundaries.
     */
    private alignTimestamp(epochSecs: number): number {
        const istTime = epochSecs + PipelineManager.IST_OFFSET;
        const aligned = Math.floor(istTime / this.timeframeSecs) * this.timeframeSecs;
        return aligned - PipelineManager.IST_OFFSET;
    }

    /** Updates the status bar OHLCV display. */
    private updateOhlcvDisplay(_tick: RawTick): void {
        const el = document.getElementById("ohlcv-display");
        if (!el) return;

        const c = this.currentCandle;
        if (c) {
            el.textContent =
                `O ${c.open.toFixed(2)}  H ${c.high.toFixed(2)}  ` +
                `L ${c.low.toFixed(2)}  C ${c.close.toFixed(2)}  ` +
                `V ${this.formatVolume(c.volume)}`;
        }
    }

    /** Formats volume with K/M/B suffixes. */
    private formatVolume(v: number): string {
        if (v >= 1_000_000_000) return (v / 1_000_000_000).toFixed(1) + "B";
        if (v >= 1_000_000) return (v / 1_000_000).toFixed(1) + "M";
        if (v >= 1_000) return (v / 1_000).toFixed(1) + "K";
        return v.toFixed(0);
    }
}
