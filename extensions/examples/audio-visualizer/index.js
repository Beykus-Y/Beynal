/// <reference path="../../../sdk/beyrix.d.ts" />
// Example Audio Visualizer extension using ONLY the public BeyrixWidgets SDK.

beyrix.background.setPreset("audio-reactive");

beyrix.audio.onFrame((ev) => {
    beyrix.ui.render(
        beyrix.ui.container(
            beyrix.ui.column(
                [
                    beyrix.ui.text(ev.beat ? "● BEAT" : "Audio Spectrum", { size: 11, color: ev.beat ? "magenta" : "fg" }),
                    beyrix.ui.progress(ev.bass, { height: 4, color: "cyan" }),
                    beyrix.ui.progress(ev.treble, { height: 4, color: "magenta" }),
                ],
                { gap: 4 }
            ),
            { padding: 8, radius: 8, bg: "card", border: "accent" }
        )
    );
});
