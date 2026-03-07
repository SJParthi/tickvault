/**
 * Smart Money Concepts Renderer — BOS/CHoCH markers on the main chart.
 *
 * Outputs from WASM:
 *   [0] Signal (1.0 = bullish break, -1.0 = bearish break, 0.0 = none)
 *   [1] Structure type (1.0 = BOS, 2.0 = CHoCH)
 *   [2] Swing level
 *   [3] Trend direction (1.0 = bullish, -1.0 = bearish)
 */

import {
    type ISeriesApi,
    type SeriesMarker,
    type Time,
} from "lightweight-charts";
import { type ChartManager } from "../chart-manager";
import { type IndicatorRenderer } from "../render-registry";

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

export class SmcRenderer implements IndicatorRenderer {
    readonly indicatorId = "smc";
    readonly instanceId: number;
    readonly displayMode = "overlay" as const;

    private levelSeries: ISeriesApi<"Line"> | null = null;
    private chartManager: ChartManager | null = null;
    private markers: SeriesMarker<Time>[] = [];

    constructor(instanceId: number) {
        this.instanceId = instanceId;
    }

    attach(chartManager: ChartManager): void {
        this.chartManager = chartManager;
        // Dashed line for swing levels
        this.levelSeries = chartManager.addLineSeries("rgba(41, 98, 255, 0.4)", 1);
    }

    update(outputs: number[], time: number): void {
        if (!this.chartManager) return;
        const signal = outputs[0];
        const structureType = outputs[1];
        const swingLevel = outputs[2];

        // Skip NaN values
        if (signal !== signal) return;

        // Update swing level line
        if (this.levelSeries && swingLevel === swingLevel) {
            this.levelSeries.update({
                time: time as Time,
                value: swingLevel,
            });
        }

        // Only show markers on actual signals
        if (signal === 0) return;

        const isBullish = signal > 0;
        const isCHoCH = structureType === 2.0;
        const label = isCHoCH ? "CHoCH" : "BOS";
        const color = isBullish ? "#26a69a" : "#ef5350";

        this.markers.push({
            time: time as Time,
            position: isBullish ? "belowBar" : "aboveBar",
            color,
            shape: isBullish ? "arrowUp" : "arrowDown",
            text: label,
        });

        // Keep last 100 markers
        if (this.markers.length > 100) {
            this.markers = this.markers.slice(-100);
        }

        this.chartManager.getCandleSeries().setMarkers(
            [...this.markers].sort((a, b) => (a.time as number) - (b.time as number))
        );
    }

    detach(): void {
        if (this.levelSeries && this.chartManager) {
            this.chartManager.getChart().removeSeries(this.levelSeries);
            this.levelSeries = null;
        }
        if (this.chartManager) {
            this.chartManager.getCandleSeries().setMarkers([]);
        }
        this.markers = [];
    }
}

/** Factory function for the render registry. */
export function createSmcRenderer(instanceId: number): IndicatorRenderer {
    return new SmcRenderer(instanceId);
}
