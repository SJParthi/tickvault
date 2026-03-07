/**
 * Supertrend Renderer — Overlay line that changes color on trend direction.
 *
 * Outputs from WASM:
 *   [0] Line value (Supertrend level)
 *   [1] Direction (1.0 = bullish, -1.0 = bearish)
 *   [2] Signal (1.0 = buy, -1.0 = sell, 0.0 = hold)
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

export class SupertrendRenderer implements IndicatorRenderer {
    readonly indicatorId = "supertrend";
    readonly instanceId: number;
    readonly displayMode = "overlay" as const;

    private lineSeries: ISeriesApi<"Line"> | null = null;
    private chartManager: ChartManager | null = null;
    private markers: SeriesMarker<Time>[] = [];
    private lastDirection = 0;

    constructor(instanceId: number) {
        this.instanceId = instanceId;
    }

    attach(chartManager: ChartManager): void {
        this.chartManager = chartManager;
        this.lineSeries = chartManager.addLineSeries("#26a69a", 2);
    }

    update(outputs: number[], time: number): void {
        if (!this.lineSeries || !this.chartManager) return;
        const value = outputs[0];
        const direction = outputs[1];
        const signal = outputs[2];

        // Skip NaN values (warmup period)
        if (value !== value) return; // NaN check

        // Line color follows trend direction
        const color = direction > 0 ? "#26a69a" : "#ef5350";
        this.lineSeries.update({
            time: time as Time,
            value,
            color,
        });

        // Buy/Sell markers on direction change
        if (signal !== 0 && direction !== this.lastDirection && this.lastDirection !== 0) {
            const isBuy = signal > 0;
            this.markers.push({
                time: time as Time,
                position: isBuy ? "belowBar" : "aboveBar",
                color: isBuy ? "#26a69a" : "#ef5350",
                shape: isBuy ? "arrowUp" : "arrowDown",
                text: isBuy ? "BUY" : "SELL",
            });

            // Keep last 200 markers
            if (this.markers.length > 200) {
                this.markers = this.markers.slice(-200);
            }

            this.chartManager.getCandleSeries().setMarkers(
                [...this.markers].sort((a, b) => (a.time as number) - (b.time as number))
            );
        }
        this.lastDirection = direction;
    }

    detach(): void {
        if (this.lineSeries && this.chartManager) {
            this.chartManager.getChart().removeSeries(this.lineSeries);
            this.lineSeries = null;
        }
        if (this.chartManager) {
            this.chartManager.getCandleSeries().setMarkers([]);
        }
        this.markers = [];
    }
}

/** Factory function for the render registry. */
export function createSupertrendRenderer(instanceId: number): IndicatorRenderer {
    return new SupertrendRenderer(instanceId);
}
