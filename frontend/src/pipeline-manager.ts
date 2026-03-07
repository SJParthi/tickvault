/**
 * Pipeline Manager — orchestrates tick → candle → indicator → render flow.
 *
 * Receives raw ticks from WsClient, aggregates into candles,
 * and dispatches to active indicator renderers.
 *
 * Future: WASM bridge for indicator computation.
 * Current: JavaScript-side aggregation for initial scaffold.
 */

import { ChartManager, type OhlcvCandle } from "./chart-manager";
import { type RawTick } from "./ws-client";

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

    /** IST offset in seconds (+5:30). */
    private static readonly IST_OFFSET = 19800;

    constructor(chartManager: ChartManager) {
        this.chartManager = chartManager;
    }

    /** Process a single live tick. */
    onTick(tick: RawTick): void {
        const aligned = this.alignTimestamp(tick.timestamp);

        if (this.currentCandle === null) {
            // First tick — start new candle
            this.currentCandle = {
                time: aligned,
                open: tick.ltp,
                high: tick.ltp,
                low: tick.ltp,
                close: tick.ltp,
                volume: tick.volume,
            };
        } else if (this.currentCandle.time === aligned) {
            // Same period — update candle
            if (tick.ltp > this.currentCandle.high) this.currentCandle.high = tick.ltp;
            if (tick.ltp < this.currentCandle.low) this.currentCandle.low = tick.ltp;
            this.currentCandle.close = tick.ltp;
            this.currentCandle.volume = tick.volume;
        } else {
            // New period — close previous candle, start new one
            this.currentCandle = {
                time: aligned,
                open: tick.ltp,
                high: tick.ltp,
                low: tick.ltp,
                close: tick.ltp,
                volume: tick.volume,
            };
        }

        // Update the chart with the current candle state
        this.chartManager.updateCandle(this.currentCandle);
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
    }

    /** Get current timeframe in seconds. */
    getTimeframe(): number {
        return this.timeframeSecs;
    }

    // ---------------------------------------------------------------------------
    // Private
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
}
