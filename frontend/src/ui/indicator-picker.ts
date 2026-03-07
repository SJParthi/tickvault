/**
 * Indicator Picker — Dropdown for adding/removing indicators.
 *
 * Appears below the "Indicators" toolbar button.
 * Shows all registered indicators with checkboxes.
 */

import { type IndicatorMeta } from "../wasm-bridge";

// ---------------------------------------------------------------------------
// Indicator Picker
// ---------------------------------------------------------------------------

export class IndicatorPicker {
    private overlay: HTMLElement;
    private list: HTMLElement;
    private visible = false;

    constructor(
        private onToggle: (id: string, active: boolean) => void,
    ) {
        // Overlay container
        this.overlay = document.createElement("div");
        this.overlay.className = "indicator-picker-overlay";
        this.overlay.style.display = "none";

        // Header
        const header = document.createElement("div");
        header.className = "indicator-picker-header";
        header.textContent = "Indicators";
        this.overlay.appendChild(header);

        // Scrollable list
        this.list = document.createElement("div");
        this.list.className = "indicator-picker-list";
        this.overlay.appendChild(this.list);

        document.body.appendChild(this.overlay);

        // Close on click outside (delayed to avoid immediate close)
        setTimeout(() => {
            document.addEventListener("mousedown", (e) => {
                if (this.visible && !this.overlay.contains(e.target as Node)) {
                    this.hide();
                }
            });
        }, 0);
    }

    /** Shows the picker below the anchor element. */
    show(anchorEl: HTMLElement, indicators: IndicatorMeta[], activeIds: Set<string>): void {
        const rect = anchorEl.getBoundingClientRect();
        this.overlay.style.left = `${rect.left}px`;
        this.overlay.style.top = `${rect.bottom + 4}px`;

        // Build indicator list
        this.list.innerHTML = "";
        for (const ind of indicators) {
            const row = document.createElement("label");
            row.className = "indicator-picker-row";

            const checkbox = document.createElement("input");
            checkbox.type = "checkbox";
            checkbox.className = "indicator-picker-checkbox";
            checkbox.checked = activeIds.has(ind.id);
            checkbox.addEventListener("change", () => {
                this.onToggle(ind.id, checkbox.checked);
            });

            const name = document.createElement("span");
            name.className = "indicator-picker-name";
            name.textContent = ind.display_name;

            const badge = document.createElement("span");
            badge.className = `indicator-picker-badge indicator-picker-badge--${ind.display}`;
            badge.textContent = ind.display;

            row.appendChild(checkbox);
            row.appendChild(name);
            row.appendChild(badge);
            this.list.appendChild(row);
        }

        this.overlay.style.display = "block";
        this.visible = true;
    }

    /** Hides the picker. */
    hide(): void {
        this.overlay.style.display = "none";
        this.visible = false;
    }

    /** Toggles the picker visibility. */
    toggle(anchorEl: HTMLElement, indicators: IndicatorMeta[], activeIds: Set<string>): void {
        if (this.visible) {
            this.hide();
        } else {
            this.show(anchorEl, indicators, activeIds);
        }
    }
}
