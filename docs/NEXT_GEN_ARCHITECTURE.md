# Beyrix Next Generation: Architecture, SDK & X-Ray Reference

## 1. Subsystem Architecture

Beyrix Next Generation extends the Linux/Wayland terminal with four modular, strictly decoupled, and lazily initialized subsystems:

- **Unified Event Bus (`src/events.rs`)**: Bounded, non-blocking pub/sub (`DEFAULT_QUEUE_CAPACITY = 256`) with backpressure (drops oldest on overflow) and subscriber counting so producers (e.g. `AudioAnalyzer`, MPRIS polling) skip work when there are zero subscribers.
- **BeyrixWidgets (`src/widgets/`)**:
  - **Runtime (`runtime.rs`)**: Isolated `rquickjs` (QuickJS) worker thread (`beyrix-widgets-vm`) with configurable memory limit, stack limit, and `set_interrupt_handler` timeout protection. Never runs on the PTY, input, or render thread.
  - **Manifest & Ecosystem (`manifest.rs`)**: `beyrix-ext.toml` package format supporting `widget`, `background`, `effect`, `provider`, `theme`, and `bundle` with SDK semver compatibility (`sdk_version = "1.0.0"`).
  - **Capability Permissions (`permissions.rs`)**: Sensitive capabilities (`audio-capture`, `terminal-read`, `xray-inspect`, `xray-control`, `network`, `process-spawn`, `filesystem`, `environment`) are denied by default for third-party extensions.
  - **Layout Engine & Visual Editor (`layout.rs`)**: `Overlay`, `Behind`, `Dock`, `Float`, `Inline`, `Hidden` modes; multi-signal fullscreen TUI detection with safe fallback (`Overlay`/`Behind`/`Hidden`, never `Dock`/`Float`); Drag & Drop, resize, edge/center/grid snapping, HiDPI scaling, click-through, hover-reveal, and layout profiles.
  - **Declarative Native UI (`ui.rs`)**: `Text`, `Button`, `Icon`, `Image`, `Container`, `Row`, `Column`, `Grid`, `Stack`, `Progress`, `Slider`, `Canvas`, `ScrollView` emitted to `Scene` (`behind_*` / `overlay_*` layers) and rendered identically by both OpenGL ES 2.0 (`src/gl.rs`) and Software `wl_shm` (`src/render.rs`).
- **Official Widgets (`extensions/official/`)**:
  - `keyboard-layout`: Uses `beyrix.keyboard.onLayoutChange` (fed by Wayland `wl_keyboard::Event::Modifiers` + `xkbcommon` layout enumeration in `src/keyboard.rs`).
  - `media-player`: Uses `beyrix.media` + `beyrix.audio` (fed by Linux MPRIS2 via `zbus`, native `symphonia` + `cpal` local player, and `rustfft` audio analyzer).
- **Beyrix Background Engine (`src/background/`)**: Native Rust 3D scene graph (`glam`), procedural geometry + `.obj` loader, Z-buffered character-grid rasterizer, 7 presets (`torus`, `sphere`, `terrain`, `ocean`, `tunnel`, `starfield`, `audio-reactive`), and native declarative `DataBinding`s (no per-frame JS).
- **Beyrix X-Ray (`src/xray/`)**: OS-level process tree, CPU/RSS/IO monitoring, `/proc/locks` + `wchan` + `syscall` wait-reason diagnostics, post-command completion reports, privacy mode, and `(pid, starttime)` + `pidfd_send_signal` safe process actions.

All subsystems default to `enabled = false` in `~/.config/beyrix/platform.toml` so disabled features spawn zero threads, perform zero polling, and load zero JS runtimes.

---

## 2. X-Ray User Problem Matrix

| User Problem (Research Source) | Existing Tooling | OS Data Source | X-Ray Solution | Limitations | Reproducible Test |
| --- | --- | --- | --- | --- | --- |
| Silent command: hung vs computing (`unix.stackexchange.com/q/643476`, `stackoverflow.com/q/223352`) | `top`, `ps`, `strace` | `/proc/[pid]/stat` (`utime`, `stime`, state `R/S/D`) | Tracks CPU% delta + time since last terminal output; classifies `ActiveCompute` with High confidence | CPU-bound infinite loop without output looks identical to valid heavy compute | `src/xray/mod.rs` CPU/sleep tests |
| Waiting for user stdin vs hung (`askubuntu.com/q/1118109`) | `ps`, `lsof`, `strace` | `/proc/[pid]/wchan` (`n_tty_read`, `wait_woken`) + `tcgetpgrp` + `/proc/[pid]/syscall` | Classifies `WaitingStdin` when blocked in `tty_read` on foreground PGID | `wchan` may be restricted under hardened kernels/Yama | `diagnosis::tests` + `cat` |
| Waiting on file lock e.g. `rust-analyzer` blocking `cargo run` (`cargo#15396`) | `lslocks`, `fuser`, `lsof` | `/proc/locks` (`->` blocked entries + matching inode holder PID) + `/proc/[pid]/stat` | Identifies `WaitingFileLock`, lock mode, and **lock owner PID + command name** without Cargo integration | Point-in-time snapshot; transient locks < poll interval may be missed | `linux_backend_detects_real_file_lock_contention` (`flock`) |
| Waiting on child process (`stackoverflow.com/q/801604`) | `pstree`, `ps` | `/proc/[pid]/wchan` (`do_wait`/`wait4`) + PPID tree | Classifies `WaitingChild` and shows full descendant tree | Grandchild root cause requires inspecting children | `diagnosis::tests` |
| Stopped process (`Ctrl+Z` / `SIGSTOP`) | `jobs`, `ps` | `/proc/[pid]/stat` state `T` | Classifies `Stopped` and offers safe `Resume` (`SIGCONT`) | Backgrounded jobs outside shell session not tracked | `linux_backend_observes_real_sleeping_stopped_and_cpu_processes` |
| PID reuse race before signaling (`arxiv:2504.13994`) | `kill` | `(pid, starttime)` + `pidfd_open` + `pidfd_send_signal` | Verifies `(pid, starttime)` before signaling and uses `pidfd` when available | Fallback `kill(2)` has narrow TOCTOU on pre-5.1 kernels | `execute_process_action` mismatch test |
| `strace` perturbing target process (`unix.stackexchange.com/q/420288`, `so/20272845`) | `strace` | Passive `procfs` polling (`Light` & `Inspect`) | Zero ptrace attachment in `Light` and `Inspect` modes | Syscall-level `Trace` requires explicit user opt-in | Default `Light`/`Inspect` non-invasive |

---

## 3. Platform Compatibility Matrix

| Feature | Linux (Wayland / Fedora 44) | Windows (Architecture & Status) |
| --- | --- | --- |
| Terminal Core & PTY | Native `openpty` + `vte` + Wayland | ConPTY not yet implemented (`src/pty.rs`) |
| Renderer | OpenGL ES 2.0 (`gl.rs`) + Software `wl_shm` (`render.rs`) | Portable `Scene` abstraction ready; Win32 windowing pending |
| Keyboard Layout | `wl_keyboard::Event::Modifiers` + `xkbcommon` | `GetKeyboardLayout` design in `keyboard.rs` |
| Media Session Control | MPRIS2 over D-Bus (`zbus`) | GSMTC architecture slot (`media/mod.rs`) |
| Local Audio Player | `symphonia` + `cpal` (ALSA/PipeWire) | `symphonia` + `cpal` (WASAPI) portable |
| Audio FFT Analysis | `rustfft` (`AudioAnalyzer`) | `rustfft` portable |
| Background 3D Engine | `glam` + `CharRasterizer` (OpenGL & Software) | 100% portable |
| X-Ray Diagnostics | `procfs` + `/proc/locks` + `pidfd` (`xray/linux.rs`) | `XRayBackend` capability model (`xray/windows.rs`, WCT/Job Objects) |

---

## 4. Configuration (`~/.config/beyrix/platform.toml`)

```toml
[widgets]
enabled = true
memory_limit_mb = 16
stack_limit_kb = 512
execution_timeout_ms = 50
editor_hotkey = "Ctrl+Shift+E"

[background]
enabled = true
preset = "torus" # torus | sphere | terrain | ocean | tunnel | starfield | audio-reactive
fps = 30
alpha = 0.35
tui_behavior = "pause"

[media]
enabled = true
mpris_enabled = true
player_enabled = true
audio_capture_enabled = false

[xray]
enabled = true
level = "light" # light | inspect | trace
poll_interval_ms = 250
privacy_mode = true
hotkey = "Ctrl+Shift+X"
```
