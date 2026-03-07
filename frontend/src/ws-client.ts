/**
 * WebSocket Client — Binary tick data receiver.
 *
 * Connects to the Rust backend's /ws/ticks endpoint.
 * Parses binary frames and forwards tick data to the pipeline manager.
 */

import { PipelineManager } from "./pipeline-manager";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Reconnect delay in milliseconds (exponential backoff). */
const RECONNECT_INITIAL_MS = 1000;
const RECONNECT_MAX_MS = 30000;

/** WebSocket endpoint path. */
const WS_PATH = "/ws/ticks";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface RawTick {
    securityId: number;
    ltp: number;
    volume: number;
    timestamp: number; // UTC epoch seconds
    open: number;
    high: number;
    low: number;
    close: number;
}

// ---------------------------------------------------------------------------
// WebSocket Client
// ---------------------------------------------------------------------------

export class WsClient {
    private ws: WebSocket | null = null;
    private pipeline: PipelineManager;
    private reconnectDelay = RECONNECT_INITIAL_MS;
    private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    private connected = false;

    constructor(pipeline: PipelineManager) {
        this.pipeline = pipeline;
    }

    /** Connects to the WebSocket endpoint. */
    connect(): void {
        const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
        const url = `${protocol}//${window.location.host}${WS_PATH}`;

        this.ws = new WebSocket(url);
        this.ws.binaryType = "arraybuffer";

        this.ws.onopen = () => {
            this.connected = true;
            this.reconnectDelay = RECONNECT_INITIAL_MS;
            console.info("[WS] Connected to", url);
        };

        this.ws.onmessage = (event: MessageEvent) => {
            if (event.data instanceof ArrayBuffer) {
                this.handleBinaryFrame(event.data);
            } else if (typeof event.data === "string") {
                this.handleJsonMessage(event.data);
            }
        };

        this.ws.onclose = (event: CloseEvent) => {
            this.connected = false;
            console.warn("[WS] Disconnected:", event.code, event.reason);
            this.scheduleReconnect();
        };

        this.ws.onerror = (event: Event) => {
            console.error("[WS] Error:", event);
        };
    }

    /** Disconnects and stops reconnection. */
    disconnect(): void {
        if (this.reconnectTimer) {
            clearTimeout(this.reconnectTimer);
            this.reconnectTimer = null;
        }
        if (this.ws) {
            this.ws.close();
            this.ws = null;
        }
        this.connected = false;
    }

    /** Whether the WebSocket is currently connected. */
    isConnected(): boolean {
        return this.connected;
    }

    // ---------------------------------------------------------------------------
    // Private
    // ---------------------------------------------------------------------------

    /**
     * Parses a binary tick frame from the backend.
     *
     * The backend sends JSON-encoded tick objects over WebSocket for now.
     * When the binary protocol is implemented, this will parse raw bytes
     * using DataView — matching the Dhan V2 binary format.
     */
    private handleBinaryFrame(buffer: ArrayBuffer): void {
        // For initial implementation, backend sends JSON over binary WS
        try {
            const text = new TextDecoder().decode(buffer);
            const tick = JSON.parse(text) as RawTick;
            this.pipeline.onTick(tick);
        } catch {
            // Future: parse binary format directly
            console.debug("[WS] Binary frame, length:", buffer.byteLength);
        }
    }

    private handleJsonMessage(data: string): void {
        try {
            const msg = JSON.parse(data);
            if (msg.type === "candles") {
                this.pipeline.onHistoricalCandles(msg.data);
            }
        } catch {
            console.debug("[WS] Non-JSON text message:", data.slice(0, 100));
        }
    }

    private scheduleReconnect(): void {
        if (this.reconnectTimer) return;

        console.info(`[WS] Reconnecting in ${this.reconnectDelay}ms...`);
        this.reconnectTimer = setTimeout(() => {
            this.reconnectTimer = null;
            this.connect();
        }, this.reconnectDelay);

        // Exponential backoff with cap
        this.reconnectDelay = Math.min(
            this.reconnectDelay * 2,
            RECONNECT_MAX_MS
        );
    }
}
