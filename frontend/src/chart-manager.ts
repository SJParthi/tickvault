/**
 * Chart Manager — Lightweight Charts v5 lifecycle.
 *
 * Creates and manages the main chart instance, panes, and series.
 * Provides typed methods for updating candles and managing indicator series.
 */

import {
    createChart,
    CandlestickSeries,
    LineSeries,
    HistogramSeries,
    type IChartApi,
    type ISeriesApi,
    type CandlestickData,
    type Time,
    ColorType,
} from "lightweight-charts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface OhlcvCandle {
    time: number; // UTC epoch seconds
    open: number;
    high: number;
    low: number;
    close: number;
    volume: number;
}

// ---------------------------------------------------------------------------
// Chart Manager
// ---------------------------------------------------------------------------

export class ChartManager {
    private chart: IChartApi;
    private candleSeries: ISeriesApi<"Candlestick">;
    private container: HTMLElement;

    constructor(container: HTMLElement) {
        this.container = container;

        this.chart = createChart(container, {
            layout: {
                background: { type: ColorType.Solid, color: "#0a0a0a" },
                textColor: "#d1d4dc",
                fontSize: 12,
            },
            grid: {
                vertLines: { color: "#1a1a2e" },
                horzLines: { color: "#1a1a2e" },
            },
            crosshair: {
                mode: 0, // Normal crosshair
            },
            rightPriceScale: {
                borderColor: "#2a2a3e",
                scaleMargins: { top: 0.1, bottom: 0.1 },
            },
            timeScale: {
                borderColor: "#2a2a3e",
                timeVisible: true,
                secondsVisible: false,
                rightOffset: 5,
                barSpacing: 6,
            },
            handleScroll: { vertTouchDrag: false },
        });

        // Main candlestick series
        this.candleSeries = this.chart.addSeries(CandlestickSeries, {
            upColor: "#26a69a",
            downColor: "#ef5350",
            borderUpColor: "#26a69a",
            borderDownColor: "#ef5350",
            wickUpColor: "#26a69a",
            wickDownColor: "#ef5350",
        });

        // Auto-resize
        const resizeObserver = new ResizeObserver((entries) => {
            for (const entry of entries) {
                const { width, height } = entry.contentRect;
                this.chart.applyOptions({ width, height });
            }
        });
        resizeObserver.observe(container);
    }

    /** Sets initial historical candle data. */
    setData(candles: OhlcvCandle[]): void {
        const data: CandlestickData<Time>[] = candles.map((c) => ({
            time: c.time as Time,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
        }));
        this.candleSeries.setData(data);
    }

    /** Updates the latest candle or adds a new one. */
    updateCandle(candle: OhlcvCandle): void {
        this.candleSeries.update({
            time: candle.time as Time,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
        });
    }

    /** Returns the chart API for indicator attachment. */
    getChart(): IChartApi {
        return this.chart;
    }

    /** Returns the main candle series. */
    getCandleSeries(): ISeriesApi<"Candlestick"> {
        return this.candleSeries;
    }

    /** Adds a line series overlay on the main chart. */
    addLineSeries(color: string, lineWidth: number = 2): ISeriesApi<"Line"> {
        return this.chart.addSeries(LineSeries, {
            color,
            lineWidth,
            crosshairMarkerVisible: false,
            lastValueVisible: false,
            priceLineVisible: false,
        });
    }

    /** Adds a histogram series (for pane indicators). */
    addHistogramSeries(
        upColor: string = "#26a69a",
        downColor: string = "#ef5350"
    ): ISeriesApi<"Histogram"> {
        return this.chart.addSeries(HistogramSeries, {
            color: upColor,
            priceLineVisible: false,
            lastValueVisible: false,
        });
    }

    /** Scrolls to the most recent candle. */
    scrollToRealtime(): void {
        this.chart.timeScale().scrollToRealTime();
    }

    /** Fits all visible data in the viewport. */
    fitContent(): void {
        this.chart.timeScale().fitContent();
    }

    /** Removes the chart and cleans up. */
    destroy(): void {
        this.chart.remove();
    }
}
