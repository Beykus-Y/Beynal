/**
 * BeyrixWidgets Public SDK TypeScript Declarations (v1.0.0)
 *
 * Add `/// <reference path="./beyrix.d.ts" />` at the top of your extension
 * entry file for full IDE autocompletion and type checking.
 */

declare namespace Beyrix {
  type ColorToken =
    | "bg"
    | "background"
    | "fg"
    | "foreground"
    | "accent"
    | "cursor"
    | "selection"
    | "selection_fg"
    | "muted"
    | "card"
    | "red"
    | "green"
    | "yellow"
    | "blue"
    | "magenta"
    | "cyan"
    | `#${string}`;

  type Align = "start" | "center" | "end" | "stretch";

  interface UiNode {
    kind: string;
  }

  interface TextOptions {
    size?: number;
    color?: ColorToken;
    align?: Align;
  }

  interface IconOptions {
    size?: number;
    color?: ColorToken;
  }

  interface ButtonOptions {
    bg?: ColorToken;
    color?: ColorToken;
    width?: number;
    height?: number;
    radius?: number;
  }

  interface ContainerOptions {
    padding?: number;
    bg?: ColorToken;
    border?: ColorToken;
    radius?: number;
    width?: number;
    height?: number;
  }

  interface FlexOptions {
    gap?: number;
    align?: Align;
  }

  interface ProgressOptions {
    width?: number;
    height?: number;
    color?: ColorToken;
    track?: ColorToken;
  }

  type CanvasCommand =
    | { type: "clear"; color: ColorToken }
    | { type: "fill-rect"; x: number; y: number; w: number; h: number; color: ColorToken; radius?: number }
    | { type: "fill-circle"; cx: number; cy: number; r: number; color: ColorToken }
    | { type: "line"; x1: number; y1: number; x2: number; y2: number; width: number; color: ColorToken };

  interface KeyboardLayoutEvent {
    activeIndex: number;
    activeName: string;
    shortName: string;
    layouts: string[];
  }

  interface MediaSessionEvent {
    sessionId: string;
    source: string;
    title: string;
    artist: string;
    album: string;
    playing: boolean;
    position: number;
    duration: number;
  }

  interface AudioFrameEvent {
    amplitude: number;
    rms: number;
    peak: number;
    beat: boolean;
    bass: number;
    mid: number;
    treble: number;
  }

  interface XRayUpdateEvent {
    rootPid: number;
    processCount: number;
    cpuPercent: number;
    rssBytes: number;
    stateLabel: string;
    waitingReason: string;
  }

  interface SDK {
    readonly sdkVersion: string;
    readonly extensionId: string;
    log(message: string): void;
    events: {
      on(event: string, cb: (payload: unknown) => void): void;
    };
    timers: {
      setTimeout(fn: () => void, ms: number): number;
      setInterval(fn: () => void, ms: number): number;
      clearTimer(id: number): void;
    };
    ui: {
      render(tree: UiNode): void;
      text(text: string, opts?: TextOptions): UiNode;
      icon(name: string, opts?: IconOptions): UiNode;
      button(id: string, label: string, onClick?: (ev: { id: string; kind: string; value: number }) => void, opts?: ButtonOptions): UiNode;
      container(child: UiNode, opts?: ContainerOptions): UiNode;
      row(children: UiNode[], opts?: FlexOptions): UiNode;
      column(children: UiNode[], opts?: FlexOptions): UiNode;
      grid(columns: number, children: UiNode[], opts?: { gap?: number }): UiNode;
      stack(children: UiNode[]): UiNode;
      progress(value: number, opts?: ProgressOptions): UiNode;
      slider(id: string, value: number, onChange?: (ev: { id: string; kind: string; value: number }) => void, opts?: ProgressOptions): UiNode;
      canvas(width: number, height: number, commands: CanvasCommand[]): UiNode;
      scrollView(height: number, children: UiNode[], scrollY?: number): UiNode;
    };
    keyboard: {
      onLayoutChange(cb: (ev: KeyboardLayoutEvent) => void): void;
    };
    media: {
      onSessionChange(cb: (ev: MediaSessionEvent) => void): void;
      playPause(): void;
      next(): void;
      prev(): void;
      seek(secs: number): void;
      setVolume(volume: number): void;
      playFile(path: string): void;
    };
    audio: {
      onFrame(cb: (ev: AudioFrameEvent) => void): void;
    };
    background: {
      setPreset(preset: "torus" | "sphere" | "terrain" | "ocean" | "tunnel" | "starfield" | "audio-reactive"): void;
      clearScene(): void;
      addObject(spec: {
        id: string;
        primitive: "torus" | "sphere" | "cube" | "plane" | "obj";
        obj_data?: string;
        color?: [number, number, number];
        position?: [number, number, number];
        rotation?: [number, number, number];
        scale?: [number, number, number];
      }): void;
      updateObject(id: string, patch: {
        position?: [number, number, number];
        rotation?: [number, number, number];
        scale?: [number, number, number];
        color?: [number, number, number];
      }): void;
      removeObject(id: string): void;
      setCamera(cam: { position?: [number, number, number]; target?: [number, number, number]; fov?: number; ortho?: boolean }): void;
      setLight(light: { direction?: [number, number, number]; color?: [number, number, number]; intensity?: number }): void;
      setCharset(charset: string): void;
      bind(binding: {
        id: string;
        source: "audio-bass" | "audio-mid" | "audio-treble" | "audio-rms" | "audio-peak" | "audio-beat" | "cpu-usage" | "ram-usage" | "xray-activity" | "mouse-x" | "mouse-y" | "time";
        target: "object-scale" | "rotation-speed" | "light-intensity" | "particle-impulse" | "brightness" | "wave-amplitude";
        inMin?: number;
        inMax?: number;
        outMin?: number;
        outMax?: number;
        smoothing?: number;
        combine?: "replace" | "add" | "multiply" | "max";
      }): void;
      unbind(id: string): void;
    };
    xray: {
      onUpdate(cb: (ev: XRayUpdateEvent) => void): void;
    };
    theme: {
      readonly name: string;
      onChange(cb: (ev: { name: string }) => void): void;
    };
    window: {
      readonly width: number;
      readonly height: number;
      readonly scale: number;
      readonly focused: boolean;
    };
  }
}

declare const beyrix: Beyrix.SDK;
