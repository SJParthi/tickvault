/**
 * Indicator Controller — Manages active indicator lifecycle.
 *
 * Connects: WASM engine ←→ Render registry ←→ DOM pane containers
 *
 * Handles adding/removing indicators, creating/destroying renderers,
 * and managing pane containers for sub-chart indicators.
 */

import { type ChartManager } from "./chart-manager";
import { type IndicatorRenderer, type RenderRegistry } from "./render-registry";
import { type WasmBridge } from "./wasm-bridge";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ActiveEntry {
    id: string;
    renderer: IndicatorRenderer;
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

export class IndicatorController {
    private active = new Map<number, ActiveEntry>();
    private wasm: WasmBridge | null = null;

    constructor(
        private chartManager: ChartManager,
        private renderRegistry: RenderRegistry,
        private chartArea: HTMLElement,
    ) {}

    /** Sets the WASM engine (may be null if not loaded). */
    setWasmBridge(wasm: WasmBridge | null): void {
        this.wasm = wasm;
    }

    /**
     * Adds an indicator by ID. Returns instance ID or null on failure.
     *
     * Creates the WASM compute instance and the visual renderer.
     * For pane indicators, creates a sub-chart container in the DOM.
     */
    addIndicator(id: string, params: number[] = []): number | null {
        if (!this.wasm) {
            console.warn("[IndicatorController] WASM not available — cannot add indicator");
            return null;
        }

        // Check if already active (only one instance per type for now)
        if (this.isActive(id)) {
            console.info("[IndicatorController] Already active:", id);
            return null;
        }

        // Create WASM compute instance
        const instanceId = this.wasm.addIndicator(id, params);
        if (instanceId < 0) {
            console.warn("[IndicatorController] Unknown indicator:", id);
            return null;
        }

        // Create renderer
        const renderer = this.renderRegistry.create(id, instanceId);
        if (!renderer) {
            this.wasm.removeIndicator(instanceId);
            console.warn("[IndicatorController] No renderer for:", id);
            return null;
        }

        // Attach renderer
        if (renderer.displayMode === "pane") {
            const paneEl = this.createPaneContainer(instanceId);
            renderer.attach(this.chartManager, paneEl);
        } else {
            renderer.attach(this.chartManager);
        }

        this.active.set(instanceId, { id, renderer });
        return instanceId;
    }

    /**
     * Removes an indicator by indicator type ID.
     * Detaches the renderer and removes the WASM instance.
     */
    removeIndicator(indicatorId: string): void {
        for (const [instanceId, entry] of this.active) {
            if (entry.id === indicatorId) {
                entry.renderer.detach();
                this.wasm?.removeIndicator(instanceId);
                this.removePaneContainer(instanceId);
                this.active.delete(instanceId);
                return;
            }
        }
    }

    /**
     * Updates all active indicator renderers with current WASM outputs.
     * Called when a candle closes.
     */
    updateAll(closedCandleTime: number): void {
        if (!this.wasm) return;

        for (const [instanceId, entry] of this.active) {
            const outputs = this.wasm.indicatorOutput(instanceId);
            if (outputs.length > 0) {
                entry.renderer.update(outputs, closedCandleTime);
            }
        }
    }

    /** Whether the given indicator type is currently active. */
    isActive(indicatorId: string): boolean {
        for (const entry of this.active.values()) {
            if (entry.id === indicatorId) return true;
        }
        return false;
    }

    /** Returns the set of active indicator type IDs. */
    activeIds(): Set<string> {
        const ids = new Set<string>();
        for (const entry of this.active.values()) {
            ids.add(entry.id);
        }
        return ids;
    }

    /** Removes all active indicators. */
    removeAll(): void {
        for (const [instanceId, entry] of this.active) {
            entry.renderer.detach();
            this.wasm?.removeIndicator(instanceId);
            this.removePaneContainer(instanceId);
        }
        this.active.clear();
    }

    // -----------------------------------------------------------------------
    // Private
    // -----------------------------------------------------------------------

    private createPaneContainer(instanceId: number): HTMLElement {
        const paneEl = document.createElement("div");
        paneEl.className = "pane-chart";
        paneEl.id = `pane-${instanceId}`;
        this.chartArea.appendChild(paneEl);
        return paneEl;
    }

    private removePaneContainer(instanceId: number): void {
        const paneEl = document.getElementById(`pane-${instanceId}`);
        if (paneEl) {
            paneEl.remove();
        }
    }
}
