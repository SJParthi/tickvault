/**
 * Squeeze Momentum Renderer — Histogram pane with squeeze state dots.
 *
 * Outputs from WASM:
 *   [0] Momentum value
 *   [1] Squeeze state (1.0 = squeeze on, 0.0 = off)
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

export class SqueezeRenderer implements IndicatorRenderer {
    readonly indicatorId = "squeeze_momentum";
    readonly instanceId: number;
    readonly displayMode = "pane" as const;

    private paneChart: IChartApi | null = null;
    private histogramSeries: ISeriesApi<"Histogram"> | null = null;
    private zeroLine: ISeriesApi<"Line"> | null = null;
    private prevMomentum = 0;

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

        this.histogramSeries = this.paneChart.addSeries(HistogramSeries, {
            priceLineVisible: false,
            lastValueVisible: false,
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
        label.textContent = "Squeeze Momentum";
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
        if (!this.histogramSeries) return;

        const momentum = outputs[0];
        const squeezeOn = outputs[1] > 0.5;

        // Skip NaN
        if (momentum !== momentum) return;

        // Color based on momentum direction and momentum increasing/decreasing
        const increasing = Math.abs(momentum) > Math.abs(this.prevMomentum);
        let color: string;
        if (momentum >= 0) {
            color = increasing ? "#26a69a" : "#b2dfdb";
        } else {
            color = increasing ? "#ef5350" : "#ef9a9a";
        }

        // Squeeze on = darker colors
        if (squeezeOn) {
            color = momentum >= 0 ? "#00695c" : "#b71c1c";
        }

        this.histogramSeries.update({
            time: time as Time,
            value: momentum,
            color,
        });

        // Zero line
        this.zeroLine?.update({
            time: time as Time,
            value: 0,
        });

        this.prevMomentum = momentum;
    }

    detach(): void {
        if (this.paneChart) {
            this.paneChart.remove();
            this.paneChart = null;
            this.histogramSeries = null;
            this.zeroLine = null;
        }
    }
}

/** Factory function for the render registry. */
export function createSqueezeRenderer(instanceId: number): IndicatorRenderer {
    return new SqueezeRenderer(instanceId);
}
