/**
 * DLT Trading Terminal — Entry Point
 *
 * Initializes the charting engine, WebSocket connection, indicator pipeline,
 * and indicator management system. This is the single entry point bundled
 * by esbuild.
 */

import { ChartManager } from "./chart-manager";
import { WsClient } from "./ws-client";
import { PipelineManager } from "./pipeline-manager";
import { IndicatorController } from "./indicator-controller";
import { RenderRegistry } from "./render-registry";
import { Toolbar } from "./ui/toolbar";
import { getWasmBridge } from "./wasm-bridge";

// Renderer factories
import { createSupertrendRenderer } from "./renderers/supertrend-renderer";
import { createSmcRenderer } from "./renderers/smc-renderer";
import { createSqueezeRenderer } from "./renderers/squeeze-renderer";
import { createMacdRenderer } from "./renderers/macd-renderer";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CHART_CONTAINER_ID = "chart-container";
const CHART_AREA_ID = "chart-area";

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

function bootstrap(): void {
    const chartContainer = document.getElementById(CHART_CONTAINER_ID);
    if (!chartContainer) {
        throw new Error(`Element #${CHART_CONTAINER_ID} not found`);
    }

    const chartArea = document.getElementById(CHART_AREA_ID);
    if (!chartArea) {
        throw new Error(`Element #${CHART_AREA_ID} not found`);
    }

    // Initialize chart
    const chartManager = new ChartManager(chartContainer);

    // Initialize render registry with all indicator renderers
    const renderRegistry = new RenderRegistry();
    renderRegistry.register("supertrend", createSupertrendRenderer);
    renderRegistry.register("smc", createSmcRenderer);
    renderRegistry.register("squeeze_momentum", createSqueezeRenderer);
    renderRegistry.register("macd_mtf", createMacdRenderer);

    // Initialize indicator controller
    const indicatorController = new IndicatorController(chartManager, renderRegistry, chartArea);

    // Initialize pipeline (indicator compute + render orchestration)
    const pipeline = new PipelineManager(chartManager);
    pipeline.setIndicatorController(indicatorController);

    // Try to connect WASM bridge
    const wasmBridge = getWasmBridge();
    if (wasmBridge) {
        pipeline.setWasmBridge(wasmBridge);
        indicatorController.setWasmBridge(wasmBridge);
        console.info("[DLT] WASM engine loaded — indicators available");
    } else {
        console.info("[DLT] WASM not available — chart-only mode");
    }

    // Initialize WebSocket client
    const wsClient = new WsClient(pipeline);

    // Initialize toolbar
    const toolbarEl = document.getElementById("toolbar");
    if (toolbarEl) {
        const toolbar = new Toolbar(toolbarEl, chartManager, wsClient, pipeline);
        toolbar.setIndicatorController(indicatorController);
        toolbar.init();
    }

    // Connect to live data
    wsClient.connect();

    // Expose for debugging in dev console
    if (typeof window !== "undefined") {
        (window as unknown as Record<string, unknown>)["dlt"] = {
            chart: chartManager,
            pipeline,
            ws: wsClient,
            indicators: indicatorController,
            renderers: renderRegistry,
        };
    }
}

// Run when DOM is ready
if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootstrap);
} else {
    bootstrap();
}
