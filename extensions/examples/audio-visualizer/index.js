/// <reference path="../../../sdk/beyrix.d.ts" />
// Example Audio Visualizer extension using ONLY the public BeyrixWidgets SDK.
// Builds a custom 3D scene and wires native declarative data bindings (zero per-frame JS).

beyrix.background.clearScene();
beyrix.background.setCamera({ position: [0, 0.9, 4.0], target: [0, 0, 0], fov: 55 });
beyrix.background.addObject({
    id: "viz-core",
    primitive: "torus",
    color: [0.35, 0.85, 0.96],
    rotation: [0.5, 0.2, 0.0],
});
beyrix.background.bind({
    id: "bass-scale",
    source: "audio-bass",
    target: "object-scale",
    outMin: 0.85,
    outMax: 1.65,
    smoothing: 0.25,
});
beyrix.background.bind({
    id: "rms-light",
    source: "audio-rms",
    target: "light-intensity",
    outMin: 0.7,
    outMax: 2.0,
    smoothing: 0.3,
});

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
