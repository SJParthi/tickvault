/**
 * DLT Trading Terminal — Entry Point
 *
 * Initializes the charting engine, WebSocket connection, and indicator pipeline.
 * This is the single entry point bundled by esbuild.
 */

import { ChartManager } from "./chart-manager";
import { WsClient } from "./ws-client";
import { PipelineManager } from "./pipeline-manager";
import { Toolbar } from "./ui/toolbar";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CHART_CONTAINER_ID = "chart-container";
const SQUEEZE_CONTAINER_ID = "squeeze-container";
const MACD_CONTAINER_ID = "macd-container";

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

function bootstrap(): void {
    const chartContainer = document.getElementById(CHART_CONTAINER_ID);
    if (!chartContainer) {
        throw new Error(`Element #${CHART_CONTAINER_ID} not found`);
    }

    // Initialize chart
    const chartManager = new ChartManager(chartContainer);

    // Initialize pipeline (indicator compute + render orchestration)
    const pipeline = new PipelineManager(chartManager);

    // Initialize WebSocket client
    const wsClient = new WsClient(pipeline);

    // Initialize toolbar
    const toolbarEl = document.getElementById("toolbar");
    if (toolbarEl) {
        const toolbar = new Toolbar(toolbarEl, chartManager, wsClient, pipeline);
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
        };
    }
}

// Run when DOM is ready
if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootstrap);
} else {
    bootstrap();
}
