/**
 * Toolbar — Symbol selector, timeframe picker, indicator button, and controls.
 *
 * Provides the top toolbar UI for the trading terminal.
 */

import { ChartManager } from "../chart-manager";
import { WsClient } from "../ws-client";
import { PipelineManager } from "../pipeline-manager";
import { IndicatorController } from "../indicator-controller";
import { IndicatorPicker } from "./indicator-picker";
import { INDICATOR_CATALOG, type IndicatorMeta } from "../wasm-bridge";

// ---------------------------------------------------------------------------
// Timeframe definitions
// ---------------------------------------------------------------------------

interface TimeframeDef {
    label: string;
    seconds: number;
}

const TIMEFRAMES: TimeframeDef[] = [
    { label: "1s", seconds: 1 },
    { label: "5s", seconds: 5 },
    { label: "15s", seconds: 15 },
    { label: "1m", seconds: 60 },
    { label: "3m", seconds: 180 },
    { label: "5m", seconds: 300 },
    { label: "15m", seconds: 900 },
    { label: "30m", seconds: 1800 },
    { label: "1H", seconds: 3600 },
    { label: "4H", seconds: 14400 },
    { label: "1D", seconds: 86400 },
];

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

export class Toolbar {
    private container: HTMLElement;
    private _chart: ChartManager;
    private _ws: WsClient;
    private pipeline: PipelineManager;
    private indicatorController: IndicatorController | null = null;
    private indicatorPicker: IndicatorPicker | null = null;
    private activeTimeframe = 60; // 1m default

    constructor(
        container: HTMLElement,
        chart: ChartManager,
        ws: WsClient,
        pipeline: PipelineManager
    ) {
        this.container = container;
        this._chart = chart;
        this._ws = ws;
        this.pipeline = pipeline;
    }

    /** Sets the indicator controller for the picker. */
    setIndicatorController(controller: IndicatorController): void {
        this.indicatorController = controller;
    }

    /** Renders the toolbar UI. */
    init(): void {
        this.container.innerHTML = "";

        // Symbol display
        const symbolEl = document.createElement("div");
        symbolEl.className = "toolbar-symbol";
        symbolEl.textContent = "NIFTY";
        this.container.appendChild(symbolEl);

        // Separator
        this.container.appendChild(this.createSeparator());

        // Timeframe buttons
        const tfGroup = document.createElement("div");
        tfGroup.className = "toolbar-group";
        for (const tf of TIMEFRAMES) {
            const btn = document.createElement("button");
            btn.className = "toolbar-btn" + (tf.seconds === this.activeTimeframe ? " active" : "");
            btn.textContent = tf.label;
            btn.dataset["seconds"] = String(tf.seconds);
            btn.addEventListener("click", () => this.onTimeframeClick(tf.seconds, btn, tfGroup));
            tfGroup.appendChild(btn);
        }
        this.container.appendChild(tfGroup);

        // Separator
        this.container.appendChild(this.createSeparator());

        // Indicators button
        const indicatorBtn = document.createElement("button");
        indicatorBtn.className = "toolbar-btn toolbar-btn-accent";
        indicatorBtn.textContent = "Indicators";
        indicatorBtn.addEventListener("click", (e) => {
            e.stopPropagation();
            this.onIndicatorClick(indicatorBtn);
        });
        this.container.appendChild(indicatorBtn);

        // Separator
        this.container.appendChild(this.createSeparator());

        // Connection status
        const statusEl = document.createElement("div");
        statusEl.className = "toolbar-status";
        statusEl.id = "ws-status";
        statusEl.textContent = "Connecting...";
        this.container.appendChild(statusEl);

        // IST Clock
        const clockEl = document.createElement("div");
        clockEl.className = "toolbar-clock";
        clockEl.id = "ist-clock";
        this.updateClock(clockEl);
        setInterval(() => this.updateClock(clockEl), 1000);
    }

    // ---------------------------------------------------------------------------
    // Private
    // ---------------------------------------------------------------------------

    private onTimeframeClick(seconds: number, btn: HTMLButtonElement, group: HTMLElement): void {
        group.querySelectorAll(".toolbar-btn").forEach((el) => el.classList.remove("active"));
        btn.classList.add("active");

        this.activeTimeframe = seconds;
        this.pipeline.setTimeframe(seconds);
    }

    private onIndicatorClick(anchorBtn: HTMLElement): void {
        // Lazy-create the picker
        if (!this.indicatorPicker) {
            this.indicatorPicker = new IndicatorPicker((id: string, active: boolean) => {
                if (!this.indicatorController) return;
                if (active) {
                    this.indicatorController.addIndicator(id);
                } else {
                    this.indicatorController.removeIndicator(id);
                }
            });
        }

        const indicators: IndicatorMeta[] = INDICATOR_CATALOG;
        const activeIds = this.indicatorController?.activeIds() ?? new Set<string>();

        this.indicatorPicker.toggle(anchorBtn, indicators, activeIds);
    }

    private createSeparator(): HTMLElement {
        const sep = document.createElement("div");
        sep.className = "toolbar-separator";
        return sep;
    }

    private updateClock(el: HTMLElement): void {
        const now = new Date();
        const istOffset = 5.5 * 60 * 60 * 1000;
        const ist = new Date(now.getTime() + istOffset + now.getTimezoneOffset() * 60 * 1000);
        const h = String(ist.getHours()).padStart(2, "0");
        const m = String(ist.getMinutes()).padStart(2, "0");
        const s = String(ist.getSeconds()).padStart(2, "0");
        el.textContent = `${h}:${m}:${s} IST`;
    }
}
