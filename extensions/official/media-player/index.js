// Official Beyrix Widget: Media Player & Audio Visualizer (v1.1.0)
// Supports Auto / Compact / Normal / Expanded modes and MPRIS capability flags.

let session = {
    source: "No media",
    title: "Nothing playing",
    artist: "",
    playing: false,
    position: 0,
    duration: 0,
    volume: 1.0,
    canPlay: true,
    canPause: true,
    canGoNext: true,
    canGoPrevious: true,
    canSeek: false,
    availableSessions: [],
};
let audio = { bass: 0, mid: 0, treble: 0, rms: 0 };

function fmtTime(sec) {
    const s = Math.max(0, Math.floor(sec || 0));
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}:${r < 10 ? "0" : ""}${r}`;
}

function effectiveMode() {
    const mode = beyrix.settings.get("viewMode", "auto");
    if (mode !== "auto") return mode;
    if (beyrix.window.width < 720) return "compact";
    return "normal";
}

function renderWidget() {
    const mode = effectiveMode();
    const showViz = beyrix.settings.get("showVisualizer", true);
    const progress = session.duration > 0 ? Math.min(1, session.position / session.duration) : 0;
    const canToggle = session.playing ? session.canPause : session.canPlay;

    if (mode === "compact") {
        const items = [
            beyrix.ui.icon("audio", { size: 12, color: "accent" }),
            beyrix.ui.text(session.title || "No media", { size: 12, color: "fg" }),
        ];
        if (canToggle) {
            items.push(
                beyrix.ui.button("play", session.playing ? "⏸" : "▶", () => beyrix.media.playPause(), {
                    width: 28,
                    height: 22,
                    radius: 6,
                })
            );
        }
        beyrix.ui.render(
            beyrix.ui.container(beyrix.ui.row(items, { gap: 6, align: "center" }), {
                padding: 7,
                radius: 9,
                bg: "surface-elevated",
                border: "border",
            })
        );
        return;
    }

    const bars = [
        { x: 0, y: 14 - audio.bass * 14, w: 6, h: Math.max(2, audio.bass * 14), color: "cyan" },
        { x: 9, y: 14 - audio.mid * 14, w: 6, h: Math.max(2, audio.mid * 14), color: "accent" },
        { x: 18, y: 14 - audio.treble * 14, w: 6, h: Math.max(2, audio.treble * 14), color: "magenta" },
    ].map((b) => ({ type: "fill-rect", ...b, radius: 2 }));

    const header = [];
    if (session.artId && typeof beyrix.ui.image === "function") {
        header.push(beyrix.ui.image(session.artId, 28, 28, { radius: 6 }));
    } else {
        header.push(beyrix.ui.icon("audio", { size: 14, color: "accent" }));
    }
    header.push(beyrix.ui.text(session.title || "Nothing playing", { size: 12.5, color: "fg" }));
    if (showViz) {
        header.push(beyrix.ui.canvas(26, 14, bars));
    }

    const controls = [];
    if (session.canGoPrevious) {
        controls.push(
            beyrix.ui.button("prev", "⏮", () => beyrix.media.prev(), { width: 30, height: 22, radius: 6 })
        );
    }
    if (canToggle) {
        controls.push(
            beyrix.ui.button("play", session.playing ? "⏸" : "▶", () => beyrix.media.playPause(), {
                width: 34,
                height: 22,
                radius: 6,
                bg: "accent-soft",
                color: "accent",
            })
        );
    }
    if (session.canGoNext) {
        controls.push(
            beyrix.ui.button("next", "⏭", () => beyrix.media.next(), { width: 30, height: 22, radius: 6 })
        );
    }
    controls.push(
        beyrix.ui.text(`${fmtTime(session.position)} / ${fmtTime(session.duration)}`, {
            size: 10.5,
            color: "muted",
        })
    );

    const rows = [
        beyrix.ui.row(header, { gap: 7, align: "center" }),
        beyrix.ui.text(
            session.artist ? `${session.artist} · ${session.source}` : session.source,
            { size: 11, color: "muted" }
        ),
        session.canSeek
            ? beyrix.ui.slider("seek", progress, (ev) => {
                  if (session.duration > 0) beyrix.media.seek(ev.value * session.duration);
              }, { height: 10 })
            : beyrix.ui.progress(progress, { height: 5 }),
        beyrix.ui.row(controls, { gap: 6, align: "center" }),
    ];

    if (mode === "expanded" && typeof session.volume === "number") {
        rows.push(
            beyrix.ui.row(
                [
                    beyrix.ui.text("Vol", { size: 10.5, color: "muted" }),
                    beyrix.ui.slider("vol", Math.min(1, Math.max(0, session.volume)), (ev) => {
                        beyrix.media.setVolume(ev.value);
                    }, { height: 10 }),
                ],
                { gap: 6, align: "center" }
            )
        );
    }

    beyrix.ui.render(
        beyrix.ui.container(beyrix.ui.column(rows, { gap: 5 }), {
            padding: 10,
            radius: 10,
            bg: "surface-elevated",
            border: "border",
        })
    );
}

beyrix.media.onSessionChange((ev) => {
    session = Object.assign(session, ev);
    renderWidget();
});
beyrix.audio.onFrame((ev) => {
    audio = ev;
    renderWidget();
});
beyrix.settings.onChange(() => renderWidget());
beyrix.theme.onChange(() => renderWidget());
renderWidget();
