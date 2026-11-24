// Official Beyrix Widget: Keyboard Layout Indicator
// Uses only the documented public BeyrixWidgets SDK (v1.0.0).

const CONFIG = {
    mode: "always", // "always" | "on-switch"
    showFullName: false,
    hideDelayMs: 1800,
    customLabels: {
        // "English (US)": "US",
        // "Russian": "RU",
    },
    layoutColors: {
        EN: "#7aa2f7",
        RU: "#f7768e",
        DE: "#e0af68",
        FR: "#9ece6a",
        UA: "#7dcfff",
    },
};

let current = {
    activeIndex: 0,
    activeName: "Unknown",
    shortName: "--",
    layouts: [],
};
let hideTimer = 0;
let visible = true;

function labelFor(ev) {
    if (CONFIG.customLabels[ev.activeName]) {
        return CONFIG.customLabels[ev.activeName];
    }
    return CONFIG.showFullName ? ev.activeName : ev.shortName;
}

function renderWidget() {
    if (!visible) {
        beyrix.ui.render(beyrix.ui.row([]));
        return;
    }
    const label = labelFor(current);
    const accent = CONFIG.layoutColors[current.shortName] || "accent";

    beyrix.ui.render(
        beyrix.ui.container(
            beyrix.ui.row(
                [
                    beyrix.ui.icon("keyboard", { size: 13, color: accent }),
                    beyrix.ui.text(label, { size: 13, color: "fg", align: "center" }),
                ],
                { gap: 6, align: "center" }
            ),
            {
                padding: 7,
                radius: 8,
                bg: "card",
                border: accent,
            }
        )
    );
}

beyrix.keyboard.onLayoutChange((ev) => {
    current = ev;
    visible = true;
    renderWidget();
    if (CONFIG.mode === "on-switch") {
        if (hideTimer) beyrix.timers.clearTimer(hideTimer);
        hideTimer = beyrix.timers.setTimeout(() => {
            visible = false;
            renderWidget();
        }, CONFIG.hideDelayMs);
    }
});

beyrix.theme.onChange(() => renderWidget());
renderWidget();
