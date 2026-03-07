/**
 * Render Registry — Maps indicator IDs to renderer factories.
 *
 * Each indicator type has a factory that creates renderer instances.
 * Renderers handle the visual representation on the chart.
 */

import { type ChartManager } from "./chart-manager";

// ---------------------------------------------------------------------------
// Renderer Interface
// ---------------------------------------------------------------------------

/**
 * Renders indicator output on the chart.
 *
 * Lifecycle: create → attach → update (many times) → detach
 */
export interface IndicatorRenderer {
    /** Which indicator type this renders. */
    readonly indicatorId: string;
    /** Unique instance ID (from WASM engine). */
    readonly instanceId: number;
    /** Whether this renders on the main chart or in a separate pane. */
    readonly displayMode: "overlay" | "pane";

    /**
     * Attaches the renderer to a chart.
     * For overlay indicators, attaches to the main chart.
     * For pane indicators, creates a sub-chart in the provided container.
     */
    attach(chartManager: ChartManager, paneContainer?: HTMLElement): void;

    /**
     * Updates the renderer with new indicator outputs.
     * Called once per closed candle with the indicator's computed values.
     */
    update(outputs: number[], time: number): void;

    /** Removes all series/elements from the chart and cleans up. */
    detach(): void;
}

// ---------------------------------------------------------------------------
// Factory Type
// ---------------------------------------------------------------------------

/** Creates a renderer instance for a given indicator instance ID. */
export type RendererFactory = (instanceId: number) => IndicatorRenderer;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/** Central registry mapping indicator IDs to renderer factories. */
export class RenderRegistry {
    private factories = new Map<string, RendererFactory>();

    /** Registers a renderer factory for an indicator type. */
    register(indicatorId: string, factory: RendererFactory): void {
        this.factories.set(indicatorId, factory);
    }

    /** Creates a renderer for the given indicator. Returns null if unregistered. */
    create(indicatorId: string, instanceId: number): IndicatorRenderer | null {
        const factory = this.factories.get(indicatorId);
        return factory ? factory(instanceId) : null;
    }

    /** Whether a renderer is registered for this indicator. */
    has(indicatorId: string): boolean {
        return this.factories.has(indicatorId);
    }
}
