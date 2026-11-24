// BeyrixWidgets Public SDK Showcase
// Exercises every public UI primitive on a single interactive panel.

let activeTab = 0;
let toggleOn = true;
let sliderVal = 0.65;
let clicks = 0;
const spark = [0.2, 0.45, 0.35, 0.7, 0.6, 0.9, 0.5, 0.8];

function render() {
    const accent = beyrix.settings.get("accentStyle", "accent");
    const tabs = beyrix.ui.tabs("demo-tabs", ["Controls", "Visuals"], activeTab, (ev) => {
        activeTab = Math.round(ev.value);
        render();
    });

    const body = activeTab === 0
        ? [
            beyrix.ui.row([
                beyrix.ui.button("btn-inc", `Clicks: ${clicks}`, () => {
                    clicks += 1;
                    render();
                }, { width: 110, height: 24, radius: 6, bg: "accent-soft", color: accent }),
                beyrix.ui.badge(toggleOn ? "ON" : "OFF", { color: toggleOn ? "success" : "muted" }),
            ], { gap: 8, align: "center" }),
            beyrix.ui.toggle("tog-demo", "Interactive Toggle", toggleOn, () => {
                toggleOn = !toggleOn;
                render();
            }),
            beyrix.ui.text(`Slider: ${Math.round(sliderVal * 100)}%`, { size: 11, color: "muted" }),
            beyrix.ui.slider("sld-demo", sliderVal, (ev) => {
                sliderVal = ev.value;
                render();
            }, { height: 12, color: accent }),
            beyrix.ui.progress(sliderVal, { height: 6, color: accent }),
        ]
        : [
            beyrix.ui.text("Sparkline & Canvas", { size: 11.5, color: "muted" }),
            beyrix.ui.sparkline(spark, 240, 28, { color: accent }),
            beyrix.ui.canvas(240, 32, [
                { type: "fill-rect", x: 0, y: 4, w: 70, h: 22, radius: 5, color: "cyan" },
                { type: "fill-circle", cx: 110, cy: 15, r: 10, color: accent },
                { type: "line", x1: 140, y1: 15, x2: 230, y2: 15, width: 3, color: "magenta" },
            ]),
        ];

    beyrix.ui.render(
        beyrix.ui.container(
            beyrix.ui.column([
                beyrix.ui.row([
                    beyrix.ui.icon("xray", { size: 14, color: accent }),
                    beyrix.ui.text("Beyrix SDK Showcase", { size: 13.5, color: "fg" }),
                ], { gap: 6, align: "center" }),
                tabs,
                beyrix.ui.divider(),
                ...body,
            ], { gap: 8 }),
            { padding: 12, radius: 10, bg: "surface-elevated", border: "border" }
        )
    );
}

beyrix.settings.onChange(() => render());
beyrix.theme.onChange(() => render());
render();
