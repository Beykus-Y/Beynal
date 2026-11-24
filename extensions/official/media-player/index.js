// Official Beyrix Widget: Media Player & Audio Visualizer
// Uses only the documented public BeyrixWidgets SDK (v1.0.0).

let session = {
    source: "No media",
    title: "Nothing playing",
    artist: "",
    playing: false,
    position: 0,
    duration: 0,
};
let audio = { bass: 0, mid: 0, treble: 0, rms: 0 };

function fmtTime(sec) {
    const s = Math.max(0, Math.floor(sec || 0));
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}:${r < 10 ? "0" : ""}${r}`;
}

function renderWidget() {
    const progress = session.duration > 0 ? Math.min(1, session.position / session.duration) : 0;
    const bars = [
        { x: 0, y: 14 - audio.bass * 14, w: 6, h: Math.max(2, audio.bass * 14), color: "cyan" },
        { x: 9, y: 14 - audio.mid * 14, w: 6, h: Math.max(2, audio.mid * 14), color: "accent" },
        { x: 18, y: 14 - audio.treble * 14, w: 6, h: Math.max(2, audio.treble * 14), color: "magenta" },
    ].map(b => ({ type: "fill-rect", ...b, radius: 2 }));

    beyrix.ui.render(
        beyrix.ui.container(
            beyrix.ui.column(
                [
                    beyrix.ui.row(
                        [
                            beyrix.ui.icon("audio", { size: 14, color: "accent" }),
                            beyrix.ui.text(session.title || "Nothing playing", { size: 13, color: "fg" }),
                            beyrix.ui.canvas(26, 14, bars),
                        ],
                        { gap: 8, align: "center" }
                    ),
                    beyrix.ui.text(
                        session.artist ? `${session.artist} · ${session.source}` : session.source,
                        { size: 11, color: "muted" }
                    ),
                    beyrix.ui.progress(progress, { height: 5 }),
                    beyrix.ui.row(
                        [
                            beyrix.ui.button("prev", "⏮", () => beyrix.media.prev(), { width: 32, height: 24, radius: 6 }),
                            beyrix.ui.button("play", session.playing ? "⏸" : "▶", () => beyrix.media.playPause(), { width: 36, height: 24, radius: 6 }),
                            beyrix.ui.button("next", "⏭", () => beyrix.media.next(), { width: 32, height: 24, radius: 6 }),
                            beyrix.ui.text(`${fmtTime(session.position)} / ${fmtTime(session.duration)}`, { size: 11, color: "muted" }),
                        ],
                        { gap: 6, align: "center" }
                    ),
                ],
                { gap: 6 }
            ),
            { padding: 10, radius: 10, bg: "card", border: "muted" }
        )
    );
}

beyrix.media.onSessionChange((ev) => {
    session = ev;
    renderWidget();
});

beyrix.audio.onFrame((ev) => {
    audio = ev;
    renderWidget();
});

beyrix.theme.onChange(() => renderWidget());
renderWidget();
