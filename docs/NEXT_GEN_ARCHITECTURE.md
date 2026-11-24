# Beyrix Next Generation: Architecture, SDK & X-Ray Reference

## 1. Subsystem Architecture

Beyrix Next Generation extends the Linux/Wayland terminal with modular, strictly decoupled, lazily initialized subsystems:

- **Smart Keyboard & Clipboard (`src/keyboard.rs`, `src/selection.rs`, `src/wayland.rs`)**:
  - **Intentional Selection**: Plain single clicks (including clicks on empty cells) never create a copyable selection. Selection commits only on drag past the HiDPI-independent logical threshold (`4.0` logical px), cell transition, double-click (word), triple-click (logical line), or keyboard range selection.
  - **Smart `Ctrl+C`**: Copies when an intentional non-empty selection exists (and by default clears selection so the next `Ctrl+C` immediately interrupts); otherwise sends literal `0x03` to the PTY so kernel `termios` (`VINTR`/`ISIG`) or raw-mode apps handle it natively. `Ctrl+Shift+C` is guaranteed copy; `force_ctrl_c_hotkey` (default `Ctrl+Alt+C`) always sends `0x03`.
  - **Smart `Ctrl+V`**: Pastes in normal shell when clipboard text is available; automatically forwards literal `0x16` in fullscreen TUI apps or when clipboard has no text offer. Configurable via `ctrl_v_policy` (`smart` | `always-paste` | `always-literal`). `Ctrl+Shift+V` is guaranteed paste; `literal_ctrl_v_hotkey` (default `Ctrl+Alt+V`) always sends `0x16`. Bracketed paste strips embedded `\x1b[200~`/`\x1b[201~` markers; unbracketed multiline pastes are guarded by a 2-press confirmation.
  - **Wayland Autorepeat**: Full `wl_keyboard::Event::RepeatInfo` support using `xkb_keymap_key_repeats`, dynamically capped `poll` timeout, live layout switching mid-hold, and immediate cancellation on key release or focus loss.
  - **Full Keyboard Audit**: Layout-independent `Ctrl+A..Z` in non-Latin layouts (e.g. Russian), `Shift+Arrows`, `Alt+key` ESC-prefixing, `Shift+Tab` (`CSI Z`), and disabled-subsystem hotkey passthrough.

- **Unified Event Bus (`src/events.rs`)**: Bounded, non-blocking pub/sub (`DEFAULT_QUEUE_CAPACITY = 256`) with backpressure and subscriber counting so producers skip work when idle.

- **BeyrixWidgets (`src/widgets/`)**:
  - **Non-blocking Lifecycle (`runtime.rs`)**: Dedicated unbounded control channel (`LifecycleCommand`) prioritized ahead of the bounded UI event channel (`VmCommand`), ensuring `Load`/`Unload`/`Reload`/`UpdateSettings`/`Shutdown` never block the UI/PTY thread and never drop under event flood. Render snapshots and `HostAction` envelopes carry host-assigned generation IDs so stale snapshots/actions are discarded.
  - **Isolation & Security (`manifest.rs`, `permissions.rs`)**: Hard UI tree caps (`MAX_UI_NODES = 512`, `MAX_UI_DEPTH = 24`, `MAX_UI_JSON_BYTES = 256 KiB`), symlink canonicalization preventing escapes outside extension root, host-determined `ExtensionTrust` (never trusting manifest `official = true`), and protection against third-party ID collisions with host-official extensions.
  - **Adaptive Layout Engine (`layout.rs`)**: Breakpoints (`Micro`/`Narrow`/`Medium`/`Wide`), `min_width`/`max_width`/`min_height`/`max_height`, bounded Dock insets (<= 45% width / 40% height), non-intrusive overlap avoidance (`avoid_overlap`), multi-signal fullscreen TUI detection, `ProfileStore` (`Minimal`, `Development`, `Music`, `Diagnostics` + CRUD + TOML import/export), and `VisualLayoutEditor` with Undo/Redo, edge/center/neighbor snapping, and live alignment guides.
  - **Native UI & Unified Settings (`ui.rs`, `editor_ui.rs`)**: Semantic theme tokens (`surface`, `surface-elevated`, `surface-hover`, `border`, `border-subtle`, `success`, `warning`, `error`, `info`, `accent-soft`), ellipsis truncation (`…`), hover feedback, `Toggle`, `Divider`, `Badge`, `Tabs`, `Sparkline`, plus a native Unified Settings Overlay (`Ctrl+,`) with atomic `platform.toml` persistence and `.bak` backups.

- **Official Widgets (`extensions/official/`)**:
  - `keyboard-layout`: Compact theme-aware indicator with `settings_schema`, initial layout replay on load, `always` / `on-switch` modes, per-layout colors, and click-through default.
  - `media-player`: Adaptive `Auto` / `Compact` / `Normal` / `Expanded` modes, MPRIS2 `SetPosition` (with `mpris:trackid`) + `ArtCache` + capability-gated buttons, local `symphonia` + `cpal` player with `local_library_dir` scanning, and live audio visualizer.

- **PipeWire System Audio Capture (`src/media/system_audio.rs`, `src/media/audio.rs`)**:
  - Opt-in (`audio_capture_enabled = true`) Linux monitor capture via dynamically loaded `libpulse-simple.so.0` (native PipeWire `pipewire-pulse` & PulseAudio).
  - Pre-allocated PCM & FFT scratch buffers; skips FFT completely when neither JS widgets nor Background Engine are consuming audio; never writes to disk or network; honest `SystemAudioStatus` capability reporting.

- **Beyrix Background Engine (`src/background/`)**:
  - Native 3D scene graph (`glam`), procedural meshes + `.obj` loader, Z-buffered character-grid rasterizer with `Fit` / `Fill` / `FixedScale` / `Responsive` viewport adaptation (preventing horizontal clipping in narrow Niri columns), frame glyph caching independent of terminal refresh rate, and safety caps (`MAX_BG_OBJECTS = 32`, `MAX_BG_TRIANGLES_PER_OBJECT = 4096`, `MAX_BG_TOTAL_TRIANGLES = 16384`, `MAX_BG_BINDINGS = 32`).

- **Beyrix X-Ray (`src/xray/`)**:
  - Independent of JavaScript VM; updates without PTY activity.
  - **Command-Level Wait-Chain Diagnosis**: Walks `bash -> cargo -> rustc` so active leaf computation is never misreported as sleeping parent.
  - **4-Tab Diagnostic UI**: Adaptive compact badge, `Overview` (wait chain, CPU & RSS sparklines, explicitly marked `~approx (procfs)` I/O), `Processes` (scrollable tree, per-PID detail, safe `pidfd` Pause/Resume/Terminate/Kill with 2-click confirmation; unsafe Renice disabled), `History` (`CommandReport` viewer), and `Trace` (honest unprivileged capability status).

All subsystems default to `enabled = false` in `~/.config/beyrix/platform.toml` for zero overhead when unused.

---

## 2. X-Ray User Problem Matrix

| User Problem | OS Data Source | X-Ray Diagnosis | Limitations |
| --- | --- | --- | --- |
| Silent command: hung vs computing | `/proc/[pid]/stat` (`utime`, `stime`, state `R/S/D`) | `ActiveCompute` + wait-chain attribution (`bash → cargo → rustc`) | Infinite CPU loop without stdout looks like heavy compute |
| Waiting for user stdin | `/proc/[pid]/wchan` (`n_tty_read`, `wait_woken`) + `tcgetpgrp` + `/proc/[pid]/syscall` | `WaitingStdin` (High/Medium confidence) | `wchan` may be restricted under hardened kernels/Yama |
| File lock contention (e.g. Cargo + `rust-analyzer`) | `/proc/locks` (`->` blocked + holder inode) + `/proc/[pid]/stat` | `WaitingFileLock` with lock mode and holder PID/comm | Point-in-time snapshot; sub-poll transient locks may be missed |
| Waiting on child process | `/proc/[pid]/wchan` (`do_wait`/`wait4`) + PPID tree | `WaitingChild` + full descendant chain | — |
| Stopped process (`Ctrl+Z` / `SIGSTOP`) | `/proc/[pid]/stat` state `T` | `Stopped` + safe `pidfd` Resume | Background jobs outside PTY session not tracked |
| I/O accounting | `/proc/[pid]/io` + 3-generation `IoAccumulator` | Marked `~approx (procfs)` in UI & reports | Passive polling cannot atomically snapshot short-lived subtrees |
| Syscall tracing (`Trace`) | Capability probe (`trace_syscalls = false` unprivileged) | Honest status in Trace tab explaining Yama/CAP_BPF | Never runs as root; never bypasses SELinux/Yama |

---

## 3. Actual Configuration Reference (`~/.config/beyrix/platform.toml`)

Every field below corresponds directly to `src/config.rs`:

```toml
[keyboard]
clear_selection_after_copy = true
ctrl_v_policy = "smart" # smart | always-paste | always-literal
multiline_paste_protection = true
force_ctrl_c_hotkey = "Ctrl+Alt+C"
literal_ctrl_v_hotkey = "Ctrl+Alt+V"
settings_hotkey = "Ctrl+,"

[widgets]
enabled = true
active_profile = "Development"
hot_reload = true
memory_limit_mb = 16
stack_limit_kb = 512
execution_timeout_ms = 50
editor_hotkey = "Ctrl+Shift+E"

[background]
enabled = true
preset = "torus" # torus | sphere | terrain | ocean | tunnel | starfield | audio-reactive
fit_mode = "fit" # fit | fill | fixed-scale | responsive
fps = 30
resolution_scale = 1.0
char_density = 1.0
char_size = 16.0
charset = " .:-=+*#%@"
palette = "theme"
speed = 1.0
alpha = 0.35
effect_intensity = 1.0
mouse_reactive = true
audio_reactive = false
pause_on_unfocus = true
pause_on_minimize = true
tui_behavior = "pause" # pause | hide | keep
auto_quality = true

[media]
enabled = true
mpris_enabled = true
player_enabled = true
audio_capture_enabled = false # opt-in PipeWire/PulseAudio monitor capture
# audio_capture_source = "@DEFAULT_MONITOR@"
# local_library_dir = "/home/user/Music"
save_state = false
fft_bands = 32
smoothing = 0.65

[xray]
enabled = true
level = "light" # light | inspect | trace
poll_interval_ms = 250
history_limit = 50
max_report_bytes = 2097152
privacy_mode = true
hotkey = "Ctrl+Shift+X"
compact_indicator = true
```
