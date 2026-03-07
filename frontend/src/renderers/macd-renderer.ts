/**
 * MACD MTF Renderer — Lines + histogram in a separate pane.
 *
 * Outputs from WASM:
 *   [0] MACD line
 *   [1] Signal line
 *   [2] Histogram
 *   [3] Zero line (always 0)
 *
 * Renders in a separate sub-chart pane below the main chart.
 */

import {
    createChart,
    HistogramSeries,
    LineSeries,
    type IChartApi,
    type ISeriesApi,
    type Time,
    ColorType,
} from "lightweight-charts";
import { type ChartManager } from "../chart-manager";
import { type IndicatorRenderer } from "../render-registry";

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

export class MacdRenderer implements IndicatorRenderer {
    readonly indicatorId = "macd_mtf";
    readonly instanceId: number;
    readonly displayMode = "pane" as const;

    private paneChart: IChartApi | null = null;
    private histogram: ISeriesApi<"Histogram"> | null = null;
    private macdLine: ISeriesApi<"Line"> | null = null;
    private signalLine: ISeriesApi<"Line"> | null = null;
    private zeroLine: ISeriesApi<"Line"> | null = null;

    constructor(instanceId: number) {
        this.instanceId = instanceId;
    }

    attach(_chartManager: ChartManager, paneContainer?: HTMLElement): void {
        if (!paneContainer) return;

        this.paneChart = createChart(paneContainer, {
            layout: {
                background: { type: ColorType.Solid, color: "#0a0a0a" },
                textColor: "#787b86",
                fontSize: 10,
            },
            grid: {
                vertLines: { color: "#1a1a2e" },
                horzLines: { color: "#1a1a2e" },
            },
            rightPriceScale: {
                borderColor: "#2a2a3e",
                scaleMargins: { top: 0.1, bottom: 0.1 },
            },
            timeScale: {
                visible: false,
            },
            crosshair: {
                mode: 0,
            },
            handleScroll: { vertTouchDrag: false },
        });

        // Histogram (behind lines)
        this.histogram = this.paneChart.addSeries(HistogramSeries, {
            priceLineVisible: false,
            lastValueVisible: false,
        });

        // MACD line
        this.macdLine = this.paneChart.addSeries(LineSeries, {
            color: "#2962ff",
            lineWidth: 1,
            crosshairMarkerVisible: false,
            lastValueVisible: false,
            priceLineVisible: false,
        });

        // Signal line
        this.signalLine = this.paneChart.addSeries(LineSeries, {
            color: "#ff6d00",
            lineWidth: 1,
            crosshairMarkerVisible: false,
            lastValueVisible: false,
            priceLineVisible: false,
        });

        // Zero reference line
        this.zeroLine = this.paneChart.addSeries(LineSeries, {
            color: "#434651",
            lineWidth: 1,
            crosshairMarkerVisible: false,
            lastValueVisible: false,
            priceLineVisible: false,
        });

        // Label
        const label = document.createElement("div");
        label.className = "pane-label";
        label.textContent = "MACD (12, 26, 9)";
        paneContainer.style.position = "relative";
        paneContainer.appendChild(label);

        // Resize handling
        const ro = new ResizeObserver((entries) => {
            for (const entry of entries) {
                const { width, height } = entry.contentRect;
                this.paneChart?.applyOptions({ width, height });
            }
        });
        ro.observe(paneContainer);
    }

    update(outputs: number[], time: number): void {
        const macdVal = outputs[0];
        const signalVal = outputs[1];
        const histVal = outputs[2];

        // Skip NaN
        if (macdVal !== macdVal) return;

        const t = time as Time;

        // Histogram color: green when positive, red when negative
        if (this.histogram) {
            this.histogram.update({
                time: t,
                value: histVal,
                color: histVal >= 0 ? "rgba(38, 166, 154, 0.5)" : "rgba(239, 83, 80, 0.5)",
            });
        }

        this.macdLine?.update({ time: t, value: macdVal });
        this.signalLine?.update({ time: t, value: signalVal });
        this.zeroLine?.update({ time: t, value: 0 });
    }

    detach(): void {
        if (this.paneChart) {
            this.paneChart.remove();
            this.paneChart = null;
            this.histogram = null;
            this.macdLine = null;
            this.signalLine = null;
            this.zeroLine = null;
        }
    }
}

/** Factory function for the render registry. */
export function createMacdRenderer(instanceId: number): IndicatorRenderer {
    return new MacdRenderer(instanceId);
}
