/// <reference path="../../../sdk/beyrix.d.ts" />
// Example third-party widget using ONLY the documented public BeyrixWidgets SDK.

let latest = {
    processCount: 0,
    cpuPercent: 0,
    rssBytes: 0,
    waitingReason: "Idle",
};

function render() {
    const mb = (latest.rssBytes / (1024 * 1024)).toFixed(1);
    beyrix.ui.render(
        beyrix.ui.container(
            beyrix.ui.column(
                [
                    beyrix.ui.row(
                        [
                            beyrix.ui.icon("xray", { size: 13, color: "cyan" }),
                            beyrix.ui.text(
                                `Procs: ${latest.processCount} · ${latest.cpuPercent.toFixed(1)}% · ${mb} MB`,
                                { size: 11.5, color: "fg" }
                            ),
                        ],
                        { gap: 6, align: "center" }
                    ),
                    beyrix.ui.text(latest.waitingReason, { size: 10.5, color: "muted" }),
                ],
                { gap: 4 }
            ),
            { padding: 8, radius: 8, bg: "card", border: "cyan" }
        )
    );
}

beyrix.xray.onUpdate((ev) => {
    latest = ev;
    render();
});

render();
