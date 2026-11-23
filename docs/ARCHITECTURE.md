# Beyrix architecture (current Linux/Wayland implementation)

This is a map of the **existing** code, not a promise of future interfaces. Beyrix currently builds a single Rust binary targeting Linux/Wayland and launches an interactive Bash session through a PTY.

| Module | Current responsibility |
| --- | --- |
| `src/main.rs` | CLI entry point, `--help`, `theme` commands, PTY mode and graphical entry point |
| `src/wayland.rs` | Wayland connection, surface, events, shared-memory buffers, input and interaction with the terminal state |
| `src/terminal.rs` | Terminal grid, escape-sequence interpretation via `vte`, scrollback and screen state |
| `src/render.rs` | CPU text rendering and cell geometry, using `fontdue` |
| `src/keyboard.rs` | Keyboard mapping and key sequence translation using `xkbcommon` |
| `src/pty.rs` | PTY process creation, Bash launch and resize |
| `src/selection.rs` | Selection position and word/line selection behavior |
| `src/theme.rs` | Themes, TOML configuration and persisted theme changes |
| `src/control.rs` | Local Unix datagram control socket for in-session theme updates |
| `src/prompt.rs` and `src/prompt_*.sh` | Bash prompt integration and its tests |

## Current boundaries and constraints

- The graphical backend is Wayland-specific; it is **not** a portable windowing abstraction.
- Rendering currently uses CPU and Wayland shared memory; there is no Vulkan or OpenGL backend yet.
- PTY handling currently uses Unix APIs and launches Bash. Windows ConPTY support does not exist.
- The terminal parser and grid have their own tests, but full xterm/VT compatibility has not been established.

## How to contribute safely

1. For terminal compatibility, isolate the reproducer and add a focused regression test under the relevant module.
2. For input or selection changes, consider modifiers, autorepeat, resize, clipboard and scrollback interactions.
3. For renderer changes, measure CPU, allocation behavior, frame timing and rendering correctness; preserve a working fallback.
4. For portability or a new renderer, open a design issue describing the intended boundary **before** changing multiple modules.
5. Use the CI commands in [CONTRIBUTING.md](../CONTRIBUTING.md). A passing unit suite does not replace a real Wayland smoke test.

See the [roadmap](../ROADMAP.md) for proposals and [configuration guide](../CONFIGURATION.md) for user-facing settings.
