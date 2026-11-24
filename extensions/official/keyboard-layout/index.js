// Official Beyrix Widget: Keyboard Layout Indicator (v1.1.0)
// Uses only the documented public BeyrixWidgets SDK (v1.0.0).

let current = {
    activeIndex: 0,
    activeName: "English (US)",
    shortName: "EN",
    layouts: [],
};
let hideTimer = 0;
let visible = true;

function getCfg() {
    return {
        mode: beyrix.settings.get("mode", "always"),
        showFullName: beyrix.settings.get("showFullName", false),
        hideDelayMs: beyrix.settings.get("hideDelayMs", 1800),
        enColor: beyrix.settings.get("enColor", "#7aa2f7"),
        ruColor: beyrix.settings.get("ruColor", "#f7768e"),
    };
}

function accentFor(shortName, cfg) {
    const map = {
        EN: cfg.enColor,
        RU: cfg.ruColor,
        DE: "#e0af68",
        FR: "#9ece6a",
        UA: "#7dcfff",
    };
    return map[shortName] || "accent";
}

function renderWidget() {
    if (!visible) {
        beyrix.ui.render(beyrix.ui.row([]));
        return;
    }
    const cfg = getCfg();
    const compact = beyrix.window.width < 720;
    const label = !compact && cfg.showFullName ? current.activeName : current.shortName;
    const accent = accentFor(current.shortName, cfg);

    beyrix.ui.render(
        beyrix.ui.container(
            beyrix.ui.row(
                compact
                    ? [beyrix.ui.text(label, { size: 12, color: accent, align: "center" })]
                    : [
                          beyrix.ui.icon("keyboard", { size: 12, color: accent }),
                          beyrix.ui.text(label, { size: 12.5, color: "fg", align: "center" }),
                      ],
                { gap: 5, align: "center" }
            ),
            {
                padding: 5,
                radius: 8,
                bg: "surface-elevated",
                border: accent,
            }
        )
    );
}

beyrix.keyboard.onLayoutChange((ev) => {
    current = ev;
    visible = true;
    renderWidget();
    const cfg = getCfg();
    if (cfg.mode === "on-switch") {
        if (hideTimer) beyrix.timers.clearTimer(hideTimer);
        hideTimer = beyrix.timers.setTimeout(() => {
            visible = false;
            renderWidget();
        }, cfg.hideDelayMs);
    }
});

beyrix.settings.onChange(() => {
    visible = true;
    renderWidget();
});
beyrix.theme.onChange(() => renderWidget());
if (beyrix.window && typeof beyrix.window.onResize === "function") {
    beyrix.window.onResize(() => renderWidget());
}
renderWidget();
